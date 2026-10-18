//! Task definitions and run records.

use crate::schedule::Schedule;
use chrono::{DateTime, Utc};
use dbiji_core::admin::{BackupOptions, RestoreOptions};
use dbiji_core::{DbError, DbResult, tr, trf};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

fn yes() -> bool {
    true
}

/// What a task does. Configurations of other tools (export, import,
/// migration, schema compare) are opaque JSON here; the application
/// deserialises them with the owning crate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaskKind {
    /// Run a SQL script (inline `sql` or a file) on a connection.
    SqlScript {
        /// Connection to run on.
        connection_id: String,
        /// Database to switch to first.
        #[serde(default)]
        database: Option<String>,
        /// Schema to make current first.
        #[serde(default)]
        schema: Option<String>,
        /// Inline script text.
        #[serde(default)]
        sql: Option<String>,
        /// Script file (used when `sql` is empty).
        #[serde(default)]
        script_path: Option<PathBuf>,
        /// Stop at the first failing statement.
        #[serde(default = "yes")]
        stop_on_error: bool,
        /// Commit when the script completes (manual transaction mode).
        #[serde(default = "yes")]
        commit_at_end: bool,
    },
    /// Data export (`dbiji-transfer` export job).
    Export {
        /// Connection to export from.
        connection_id: String,
        /// Serialised export job.
        config: serde_json::Value,
    },
    /// Data import (`dbiji-transfer` import job).
    Import {
        /// Connection to import into.
        connection_id: String,
        /// Serialised import job.
        config: serde_json::Value,
    },
    /// Backup with the vendor tool.
    Backup {
        /// Connection to back up.
        connection_id: String,
        /// Tool options.
        options: BackupOptions,
    },
    /// Restore with the vendor tool.
    Restore {
        /// Connection to restore into.
        connection_id: String,
        /// Tool options.
        options: RestoreOptions,
    },
    /// Table migration between two connections.
    Migration {
        /// Connection to read from.
        source_connection_id: String,
        /// Connection to write to.
        target_connection_id: String,
        /// Serialised migration job.
        config: serde_json::Value,
    },
    /// Schema comparison (and optionally applying the script).
    SchemaCompare {
        /// Serialised compare job.
        config: serde_json::Value,
    },
    /// Shell command.
    Shell {
        /// Command line run by the platform shell.
        command: String,
    },
    /// Run other tasks (by id), one after another or in parallel.
    Composite {
        /// Ids of the step tasks, in order.
        steps: Vec<String>,
        /// Stop (sequential) or cancel the others (parallel) when a step fails.
        #[serde(default = "yes")]
        stop_on_error: bool,
        /// Run all steps at the same time.
        #[serde(default)]
        parallel: bool,
    },
}

impl TaskKind {
    /// English label of the task type (routed through i18n by the UI).
    pub fn label(&self) -> &'static str {
        match self {
            TaskKind::SqlScript { .. } => "SQL script",
            TaskKind::Export { .. } => "Data export",
            TaskKind::Import { .. } => "Data import",
            TaskKind::Backup { .. } => "Backup",
            TaskKind::Restore { .. } => "Restore",
            TaskKind::Migration { .. } => "Data migration",
            TaskKind::SchemaCompare { .. } => "Schema compare",
            TaskKind::Shell { .. } => "Shell command",
            TaskKind::Composite { .. } => "Composite task",
        }
    }

    /// Connections the task uses.
    pub fn connection_ids(&self) -> Vec<&str> {
        match self {
            TaskKind::SqlScript { connection_id, .. }
            | TaskKind::Export { connection_id, .. }
            | TaskKind::Import { connection_id, .. }
            | TaskKind::Backup { connection_id, .. }
            | TaskKind::Restore { connection_id, .. } => vec![connection_id.as_str()],
            TaskKind::Migration { source_connection_id, target_connection_id, .. } => {
                vec![source_connection_id.as_str(), target_connection_id.as_str()]
            }
            TaskKind::SchemaCompare { .. } | TaskKind::Shell { .. } | TaskKind::Composite { .. } => Vec::new(),
        }
    }

    /// A [`TaskKind::Composite`] task.
    pub fn is_composite(&self) -> bool {
        matches!(self, TaskKind::Composite { .. })
    }
}

/// Desktop notification preferences of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyOptions {
    /// Notify when a run succeeds.
    pub on_success: bool,
    /// Notify when a run fails (default).
    pub on_failure: bool,
}

impl Default for NotifyOptions {
    fn default() -> Self {
        NotifyOptions { on_success: false, on_failure: true }
    }
}

/// A saved task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskDef {
    /// Stable id (UUID).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Folder path, `/` separated.
    #[serde(default)]
    pub folder: Option<String>,
    /// Free text description.
    #[serde(default)]
    pub description: Option<String>,
    /// What the task does.
    pub kind: TaskKind,
    /// When the scheduler runs it (none = manual only).
    #[serde(default)]
    pub schedule: Option<Schedule>,
    /// Disabled tasks are not run by the scheduler (they can still be run manually).
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Creation time.
    pub created: DateTime<Utc>,
    /// Last modification time.
    pub updated: DateTime<Utc>,
    /// Cancel the run after this many seconds.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Desktop notifications.
    #[serde(default)]
    pub notify: NotifyOptions,
    /// Also register the schedule with the operating system scheduler so the
    /// task runs while DBiji is closed (explicit opt-in per task).
    #[serde(default)]
    pub os_scheduler: bool,
}

impl TaskDef {
    /// New enabled task with a fresh id.
    pub fn new(name: impl Into<String>, kind: TaskKind) -> Self {
        let now = Utc::now();
        TaskDef {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            folder: None,
            description: None,
            kind,
            schedule: None,
            enabled: true,
            created: now,
            updated: now,
            timeout_secs: None,
            notify: NotifyOptions::default(),
            os_scheduler: false,
        }
    }

    /// Set the schedule.
    pub fn with_schedule(mut self, schedule: Schedule) -> Self {
        self.schedule = Some(schedule);
        self
    }

    /// Set the folder.
    pub fn with_folder(mut self, folder: impl Into<String>) -> Self {
        self.folder = Some(folder.into());
        self
    }

    /// Set the time limit.
    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        self.timeout_secs = Some(secs);
        self
    }

    /// Run time limit.
    pub fn timeout(&self) -> Option<Duration> {
        self.timeout_secs.filter(|s| *s > 0).map(Duration::from_secs)
    }

    /// `folder/name` for display.
    pub fn path(&self) -> String {
        match self.folder.as_deref().map(|f| f.trim_matches('/')).filter(|f| !f.is_empty()) {
            Some(f) => format!("{f}/{}", self.name),
            None => self.name.clone(),
        }
    }

    /// Check the definition for errors the user must fix.
    pub fn validate(&self) -> DbResult<()> {
        if self.name.trim().is_empty() {
            return Err(DbError::config(tr("The task needs a name")));
        }
        if self.id.trim().is_empty() {
            return Err(DbError::config(tr("The task needs an id")));
        }
        if self.timeout_secs == Some(0) {
            return Err(DbError::config(tr("The timeout must be at least one second")));
        }
        if let Some(s) = &self.schedule {
            s.validate().map_err(|e| DbError::config(e.to_string()))?;
        }
        let need = |v: &str, what: &str| -> DbResult<()> {
            if v.trim().is_empty() { Err(DbError::config(trf("The task needs a {}", &[&tr(what)]))) } else { Ok(()) }
        };
        match &self.kind {
            TaskKind::SqlScript { connection_id, sql, script_path, .. } => {
                need(connection_id, "connection")?;
                let has_sql = sql.as_deref().is_some_and(|s| !s.trim().is_empty());
                if !has_sql && script_path.is_none() {
                    return Err(DbError::config(tr("The SQL script task needs SQL text or a script file")));
                }
            }
            TaskKind::Export { connection_id, .. }
            | TaskKind::Import { connection_id, .. }
            | TaskKind::Backup { connection_id, .. }
            | TaskKind::Restore { connection_id, .. } => need(connection_id, "connection")?,
            TaskKind::Migration { source_connection_id, target_connection_id, .. } => {
                need(source_connection_id, "source connection")?;
                need(target_connection_id, "target connection")?;
            }
            TaskKind::SchemaCompare { .. } => {}
            TaskKind::Shell { command } => need(command, "command")?,
            TaskKind::Composite { steps, .. } => {
                if steps.is_empty() {
                    return Err(DbError::config(tr("A composite task needs at least one step")));
                }
                if steps.iter().any(|s| s == &self.id) {
                    return Err(DbError::config(tr("A composite task cannot contain itself")));
                }
            }
        }
        Ok(())
    }
}

/// What started a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// Started by the user.
    Manual,
    /// Started by the scheduler.
    Schedule,
    /// `dbiji --run-task` (also used by the OS scheduler).
    Cli,
    /// Step of a composite task.
    Composite,
}

/// State of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// In progress.
    Running,
    /// Completed without error.
    Succeeded,
    /// Failed (error or time limit).
    Failed,
    /// Cancelled by the user or by shutdown.
    Cancelled,
    /// Not run (previous run still active, step skipped after an error ...).
    Skipped,
}

impl RunStatus {
    /// Not [`RunStatus::Running`].
    pub fn is_final(self) -> bool {
        self != RunStatus::Running
    }
    /// English label (routed through i18n by the UI).
    pub fn label(self) -> &'static str {
        match self {
            RunStatus::Running => "Running",
            RunStatus::Succeeded => "Succeeded",
            RunStatus::Failed => "Failed",
            RunStatus::Cancelled => "Cancelled",
            RunStatus::Skipped => "Skipped",
        }
    }
}

/// Severity of a run message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageLevel {
    /// Information.
    Info,
    /// Something to look at.
    Warning,
    /// An error.
    Error,
}

/// A line of a run's log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMessage {
    /// When it was logged.
    pub time: DateTime<Utc>,
    /// Severity.
    pub level: MessageLevel,
    /// Message text (at most [`MAX_MESSAGE_CHARS`] characters).
    pub text: String,
}

/// Messages kept per run: the first [`KEEP_FIRST_MESSAGES`] and the most recent ones.
pub const MAX_RUN_MESSAGES: usize = 500;
/// Leading messages always kept.
pub const KEEP_FIRST_MESSAGES: usize = 50;
/// Longer messages are truncated.
pub const MAX_MESSAGE_CHARS: usize = 4000;

/// Truncate `s` to at most `max` characters, marking the cut.
pub(crate) fn truncate_text(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}\u{2026}", &s[..i]),
        None => s.to_string(),
    }
}

/// Record of one execution of a task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRun {
    /// Unique id of the run.
    pub run_id: String,
    /// Task that ran.
    pub task_id: String,
    /// Name of the task at the time of the run.
    pub task_name: String,
    /// What started the run.
    pub trigger: Trigger,
    /// Start time.
    pub started: DateTime<Utc>,
    /// End time of a finished run.
    #[serde(default)]
    pub finished: Option<DateTime<Utc>>,
    /// Current or final state.
    pub status: RunStatus,
    /// Log of the run (bounded, see [`MAX_RUN_MESSAGES`]).
    #[serde(default)]
    pub messages: Vec<RunMessage>,
    /// Messages dropped because the run produced more than [`MAX_RUN_MESSAGES`].
    #[serde(default)]
    pub dropped_messages: usize,
    /// Files written by the run.
    #[serde(default)]
    pub output_files: Vec<PathBuf>,
    /// Rows processed.
    #[serde(default)]
    pub rows: Option<u64>,
    /// Error of a failed or cancelled run.
    #[serde(default)]
    pub error: Option<String>,
    /// Composite run that started this run.
    #[serde(default)]
    pub parent_run_id: Option<String>,
    /// Process that executed the run.
    #[serde(default)]
    pub pid: u32,
}

impl TaskRun {
    /// A new run in [`RunStatus::Running`] state.
    pub fn start(task: &TaskDef, trigger: Trigger, started: DateTime<Utc>, parent_run_id: Option<String>) -> Self {
        TaskRun {
            run_id: uuid::Uuid::new_v4().to_string(),
            task_id: task.id.clone(),
            task_name: task.name.clone(),
            trigger,
            started,
            finished: None,
            status: RunStatus::Running,
            messages: Vec::new(),
            dropped_messages: 0,
            output_files: Vec::new(),
            rows: None,
            error: None,
            parent_run_id,
            pid: std::process::id(),
        }
    }

    /// Wall time of a finished run.
    pub fn duration(&self) -> Option<chrono::Duration> {
        self.finished.map(|f| f - self.started)
    }

    /// Append a message, keeping the log bounded.
    pub fn push_message(&mut self, mut m: RunMessage) {
        if m.text.chars().count() > MAX_MESSAGE_CHARS {
            m.text = truncate_text(&m.text, MAX_MESSAGE_CHARS);
        }
        if self.messages.len() >= MAX_RUN_MESSAGES {
            self.messages.remove(KEEP_FIRST_MESSAGES);
            self.dropped_messages += 1;
        }
        self.messages.push(m);
    }

    /// Process exit code for CLI runs: 0 succeeded, 1 failed, 2 cancelled, 3 skipped.
    pub fn exit_code(&self) -> i32 {
        match self.status {
            RunStatus::Succeeded => 0,
            RunStatus::Failed | RunStatus::Running => 1,
            RunStatus::Cancelled => 2,
            RunStatus::Skipped => 3,
        }
    }
}

/// Result reported by a [`crate::TaskExecutor`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunOutcome {
    /// Rows processed (exported, imported, affected ...).
    pub rows: Option<u64>,
    /// Files written (exports, backups).
    pub output_files: Vec<PathBuf>,
    /// Final summary line.
    pub message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_serde_and_defaults() {
        let t = TaskDef::new("Nightly", TaskKind::Shell { command: "echo hi".into() }).with_folder("ops/nightly").with_timeout_secs(60);
        let json = serde_json::to_string(&t).unwrap();
        assert!(json.contains("\"type\":\"shell\""));
        let back: TaskDef = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t);
        assert_eq!(t.path(), "ops/nightly/Nightly");
        assert_eq!(t.timeout(), Some(Duration::from_secs(60)));
        let minimal: TaskDef = serde_json::from_str(
            r#"{"id":"x","name":"n","kind":{"type":"composite","steps":["a"]},"created":"2026-01-01T00:00:00Z","updated":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        assert!(minimal.enabled);
        assert!(matches!(minimal.kind, TaskKind::Composite { stop_on_error: true, parallel: false, .. }));
        assert_eq!(minimal.notify, NotifyOptions::default());
    }

    #[test]
    fn validation() {
        let mut t = TaskDef::new("", TaskKind::Shell { command: "ls".into() });
        assert!(t.validate().is_err());
        t.name = "x".into();
        assert!(t.validate().is_ok());
        t.kind = TaskKind::SqlScript {
            connection_id: "c".into(),
            database: None,
            schema: None,
            sql: Some("  ".into()),
            script_path: None,
            stop_on_error: true,
            commit_at_end: true,
        };
        assert!(t.validate().is_err());
        t.kind = TaskKind::Composite { steps: vec![t.id.clone()], stop_on_error: true, parallel: false };
        assert!(t.validate().is_err());
        t.kind = TaskKind::Backup { connection_id: "c".into(), options: BackupOptions::default() };
        t.timeout_secs = Some(0);
        assert!(t.validate().is_err());
    }

    #[test]
    fn messages_are_bounded() {
        let t = TaskDef::new("x", TaskKind::Shell { command: "ls".into() });
        let mut r = TaskRun::start(&t, Trigger::Manual, Utc::now(), None);
        for i in 0..(MAX_RUN_MESSAGES + 25) {
            r.push_message(RunMessage { time: Utc::now(), level: MessageLevel::Info, text: format!("m{i}") });
        }
        assert_eq!(r.messages.len(), MAX_RUN_MESSAGES);
        assert_eq!(r.dropped_messages, 25);
        assert_eq!(r.messages[0].text, "m0");
        assert_eq!(r.messages[KEEP_FIRST_MESSAGES - 1].text, format!("m{}", KEEP_FIRST_MESSAGES - 1));
        assert_eq!(r.messages.last().unwrap().text, format!("m{}", MAX_RUN_MESSAGES + 24));
        r.push_message(RunMessage { time: Utc::now(), level: MessageLevel::Error, text: "é".repeat(MAX_MESSAGE_CHARS + 10) });
        assert_eq!(r.messages.last().unwrap().text.chars().count(), MAX_MESSAGE_CHARS + 1);
        assert_eq!(r.exit_code(), 1);
    }
}
