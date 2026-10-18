//! Editable model of a task — the state of the task editor — and its
//! conversion to and from [`TaskDef`]. Pure data without UI.

use crate::tabs::dbtools::{BackupForm, RestoreForm};
use crate::taskexec::CompareTaskConfig;
use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use dbiji_core::{tr, trf};
use dbiji_tasks::{NotifyOptions, Schedule, TaskDef, TaskKind};
use egui_phosphor::regular as icon;
use std::path::PathBuf;

/// Task types created from the task manager (export, import and migration
/// tasks are created from the transfer wizards).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewTaskKind {
    SqlScript,
    Shell,
    Backup,
    Restore,
    SchemaCompare,
    Composite,
}

impl NewTaskKind {
    pub const ALL: [NewTaskKind; 6] =
        [NewTaskKind::SqlScript, NewTaskKind::Shell, NewTaskKind::Backup, NewTaskKind::Restore, NewTaskKind::SchemaCompare, NewTaskKind::Composite];

    /// Localised label.
    pub fn label(self) -> &'static str {
        match self {
            NewTaskKind::SqlScript => tr("SQL script"),
            NewTaskKind::Shell => tr("Shell command"),
            NewTaskKind::Backup => tr("Backup"),
            NewTaskKind::Restore => tr("Restore"),
            NewTaskKind::SchemaCompare => tr("Schema compare"),
            NewTaskKind::Composite => tr("Composite task"),
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            NewTaskKind::SqlScript => icon::FILE_SQL,
            NewTaskKind::Shell => icon::TERMINAL,
            NewTaskKind::Backup => icon::ARCHIVE,
            NewTaskKind::Restore => icon::ARROW_COUNTER_CLOCKWISE,
            NewTaskKind::SchemaCompare => icon::GIT_DIFF,
            NewTaskKind::Composite => icon::STACK,
        }
    }

    /// The type needs a connection.
    pub fn uses_connection(self) -> bool {
        matches!(self, NewTaskKind::SqlScript | NewTaskKind::Backup | NewTaskKind::Restore)
    }
}

/// Localised label of a task type.
pub fn kind_label(kind: &TaskKind) -> &'static str {
    match kind {
        TaskKind::SqlScript { .. } => tr("SQL script"),
        TaskKind::Export { .. } => tr("Data export"),
        TaskKind::Import { .. } => tr("Data import"),
        TaskKind::Backup { .. } => tr("Backup"),
        TaskKind::Restore { .. } => tr("Restore"),
        TaskKind::Migration { .. } => tr("Data migration"),
        TaskKind::SchemaCompare { .. } => tr("Schema compare"),
        TaskKind::Shell { .. } => tr("Shell command"),
        TaskKind::Composite { .. } => tr("Composite task"),
    }
}

/// Icon of a task type.
pub fn kind_icon(kind: &TaskKind) -> &'static str {
    match kind {
        TaskKind::SqlScript { .. } => icon::FILE_SQL,
        TaskKind::Export { .. } => icon::EXPORT,
        TaskKind::Import { .. } => icon::DOWNLOAD_SIMPLE,
        TaskKind::Backup { .. } => icon::ARCHIVE,
        TaskKind::Restore { .. } => icon::ARROW_COUNTER_CLOCKWISE,
        TaskKind::Migration { .. } => icon::ARROWS_LEFT_RIGHT,
        TaskKind::SchemaCompare { .. } => icon::GIT_DIFF,
        TaskKind::Shell { .. } => icon::TERMINAL,
        TaskKind::Composite { .. } => icon::STACK,
    }
}

/// How a schedule is entered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleMode {
    /// Manual runs only.
    None,
    Cron,
    Interval,
    Daily,
    Once,
}

/// Unit of an interval schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntervalUnit {
    Seconds,
    Minutes,
    Hours,
    Days,
}

impl IntervalUnit {
    pub const ALL: [IntervalUnit; 4] = [IntervalUnit::Seconds, IntervalUnit::Minutes, IntervalUnit::Hours, IntervalUnit::Days];

    pub fn secs(self) -> u64 {
        match self {
            IntervalUnit::Seconds => 1,
            IntervalUnit::Minutes => 60,
            IntervalUnit::Hours => 3600,
            IntervalUnit::Days => 86_400,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            IntervalUnit::Seconds => tr("seconds"),
            IntervalUnit::Minutes => tr("minutes"),
            IntervalUnit::Hours => tr("hours"),
            IntervalUnit::Days => tr("days"),
        }
    }
}

/// Short localised weekday names, Monday first.
pub fn weekday_names() -> [&'static str; 7] {
    [tr("Mon"), tr("Tue"), tr("Wed"), tr("Thu"), tr("Fri"), tr("Sat"), tr("Sun")]
}

/// Editable schedule.
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduleDraft {
    pub mode: ScheduleMode,
    /// Cron expression.
    pub cron: String,
    /// Interval count and unit.
    pub every: u64,
    pub unit: IntervalUnit,
    /// `HH:MM` local time of a daily schedule.
    pub daily_at: String,
    /// Days of a daily schedule, Monday first; none = every day.
    pub weekdays: [bool; 7],
    /// `YYYY-MM-DD HH:MM` local time of a one-time schedule.
    pub once_at: String,
}

impl Default for ScheduleDraft {
    fn default() -> Self {
        let soon = Local::now() + chrono::Duration::hours(1);
        ScheduleDraft {
            mode: ScheduleMode::None,
            cron: "0 2 * * *".into(),
            every: 1,
            unit: IntervalUnit::Hours,
            daily_at: "02:00".into(),
            weekdays: [false; 7],
            once_at: soon.format("%Y-%m-%d %H:00").to_string(),
        }
    }
}

/// Parse `YYYY-MM-DD HH:MM[:SS]` (also with `T`) as local time.
pub fn parse_local_time(s: &str) -> Result<DateTime<Local>, String> {
    let t = s.trim();
    let naive = ["%Y-%m-%d %H:%M", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%dT%H:%M:%S"]
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(t, f).ok())
        .ok_or_else(|| trf("Invalid date and time '{}', expected YYYY-MM-DD HH:MM", &[&t]))?;
    Local.from_local_datetime(&naive).earliest().ok_or_else(|| tr("This local time does not exist (daylight saving time change)").to_string())
}

impl ScheduleDraft {
    pub fn from_schedule(s: Option<&Schedule>) -> Self {
        let mut d = ScheduleDraft::default();
        match s {
            None => {}
            Some(Schedule::Cron(e)) => {
                d.mode = ScheduleMode::Cron;
                d.cron = e.clone();
            }
            Some(Schedule::Interval { every_secs }) => {
                d.mode = ScheduleMode::Interval;
                let unit = [IntervalUnit::Days, IntervalUnit::Hours, IntervalUnit::Minutes]
                    .into_iter()
                    .find(|u| *every_secs > 0 && every_secs % u.secs() == 0)
                    .unwrap_or(IntervalUnit::Seconds);
                d.unit = unit;
                d.every = every_secs / unit.secs();
            }
            Some(Schedule::Daily { at, weekdays }) => {
                d.mode = ScheduleMode::Daily;
                d.daily_at = at.clone();
                for &w in weekdays {
                    let i = match w {
                        0 | 7 => 6,
                        n @ 1..=6 => (n - 1) as usize,
                        _ => continue,
                    };
                    d.weekdays[i] = true;
                }
            }
            Some(Schedule::Once { at }) => {
                d.mode = ScheduleMode::Once;
                d.once_at = at.format("%Y-%m-%d %H:%M").to_string();
            }
        }
        d
    }

    /// The schedule (`None` = manual only), or why it is invalid.
    pub fn to_schedule(&self) -> Result<Option<Schedule>, String> {
        let s = match self.mode {
            ScheduleMode::None => return Ok(None),
            ScheduleMode::Cron => Schedule::Cron(self.cron.split_whitespace().collect::<Vec<_>>().join(" ")),
            ScheduleMode::Interval => {
                let secs = self.every.checked_mul(self.unit.secs()).ok_or_else(|| tr("The interval must be at most one year").to_string())?;
                Schedule::Interval { every_secs: secs }
            }
            ScheduleMode::Daily => Schedule::Daily {
                at: self.daily_at.trim().to_string(),
                weekdays: (0..7).filter(|i| self.weekdays[*i]).map(|i| i as u8 + 1).collect(),
            },
            ScheduleMode::Once => Schedule::Once { at: parse_local_time(&self.once_at)? },
        };
        s.validate().map_err(|e| e.to_string())?;
        Ok(Some(s))
    }

    /// The next `n` run times after `from` (empty for manual tasks and
    /// one-time schedules in the past).
    pub fn preview(&self, from: DateTime<Local>, n: usize) -> Result<Vec<DateTime<Local>>, String> {
        let Some(s) = self.to_schedule()? else { return Ok(Vec::new()) };
        let mut out = Vec::with_capacity(n);
        let mut t = from;
        while out.len() < n {
            match s.next_after(t) {
                Some(next) => {
                    out.push(next);
                    t = next;
                }
                None => break,
            }
        }
        Ok(out)
    }
}

/// SQL script task form.
#[derive(Debug, Clone, PartialEq)]
pub struct SqlScriptDraft {
    pub connection_id: String,
    pub database: String,
    pub schema: String,
    /// Run a script file instead of the inline text.
    pub from_file: bool,
    pub sql: String,
    pub script_path: String,
    pub stop_on_error: bool,
    pub commit_at_end: bool,
}

/// Schema compare task form ([`CompareTaskConfig`]).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CompareDraft {
    pub source_connection: String,
    pub source_schema: String,
    pub target_connection: String,
    pub target_schema: String,
    pub drop_removed: bool,
    pub apply: bool,
    pub script_path: String,
    pub report_path: String,
}

fn opt(s: &str) -> Option<String> {
    Some(s.trim().to_string()).filter(|s| !s.is_empty())
}

fn opt_path(s: &str) -> Option<PathBuf> {
    opt(s).map(PathBuf::from)
}

fn path_text(p: &Option<PathBuf>) -> String {
    p.as_ref().map(|p| p.display().to_string()).unwrap_or_default()
}

impl CompareDraft {
    pub fn from_config(c: &CompareTaskConfig) -> Self {
        CompareDraft {
            source_connection: c.source_connection.clone(),
            source_schema: c.source_schema.clone().unwrap_or_default(),
            target_connection: c.target_connection.clone(),
            target_schema: c.target_schema.clone().unwrap_or_default(),
            drop_removed: c.drop_removed,
            apply: c.apply,
            script_path: path_text(&c.script_path),
            report_path: path_text(&c.report_path),
        }
    }

    pub fn to_config(&self) -> CompareTaskConfig {
        CompareTaskConfig {
            source_connection: self.source_connection.clone(),
            source_schema: opt(&self.source_schema),
            target_connection: self.target_connection.clone(),
            target_schema: opt(&self.target_schema),
            drop_removed: self.drop_removed,
            apply: self.apply,
            script_path: opt_path(&self.script_path),
            report_path: opt_path(&self.report_path),
        }
    }
}

/// What the task does, as edited in the form.
#[derive(Debug, Clone, PartialEq)]
pub enum KindDraft {
    SqlScript(SqlScriptDraft),
    Shell { command: String },
    Backup { connection_id: String, form: BackupForm },
    Restore { connection_id: String, form: RestoreForm },
    SchemaCompare(CompareDraft),
    Composite { steps: Vec<String>, stop_on_error: bool, parallel: bool },
    /// Export, import and migration tasks: edited in the transfer wizards,
    /// kept as they are.
    External(TaskKind),
}

impl KindDraft {
    /// Form of a new task, using `connection` where a connection is needed.
    pub fn new(kind: NewTaskKind, connection: Option<String>) -> Self {
        let conn = connection.unwrap_or_default();
        match kind {
            NewTaskKind::SqlScript => KindDraft::SqlScript(SqlScriptDraft {
                connection_id: conn,
                database: String::new(),
                schema: String::new(),
                from_file: false,
                sql: String::new(),
                script_path: String::new(),
                stop_on_error: true,
                commit_at_end: true,
            }),
            NewTaskKind::Shell => KindDraft::Shell { command: String::new() },
            NewTaskKind::Backup => KindDraft::Backup { connection_id: conn, form: BackupForm::default() },
            NewTaskKind::Restore => KindDraft::Restore { connection_id: conn, form: RestoreForm::default() },
            NewTaskKind::SchemaCompare => KindDraft::SchemaCompare(CompareDraft { source_connection: conn.clone(), target_connection: conn, drop_removed: true, ..Default::default() }),
            NewTaskKind::Composite => KindDraft::Composite { steps: Vec::new(), stop_on_error: true, parallel: false },
        }
    }

    pub fn from_kind(kind: &TaskKind) -> Self {
        match kind {
            TaskKind::SqlScript { connection_id, database, schema, sql, script_path, stop_on_error, commit_at_end } => {
                let has_sql = sql.as_deref().is_some_and(|s| !s.trim().is_empty());
                KindDraft::SqlScript(SqlScriptDraft {
                    connection_id: connection_id.clone(),
                    database: database.clone().unwrap_or_default(),
                    schema: schema.clone().unwrap_or_default(),
                    from_file: !has_sql && script_path.is_some(),
                    sql: sql.clone().unwrap_or_default(),
                    script_path: path_text(script_path),
                    stop_on_error: *stop_on_error,
                    commit_at_end: *commit_at_end,
                })
            }
            TaskKind::Shell { command } => KindDraft::Shell { command: command.clone() },
            TaskKind::Backup { connection_id, options } => KindDraft::Backup { connection_id: connection_id.clone(), form: BackupForm::from_options(options) },
            TaskKind::Restore { connection_id, options } => KindDraft::Restore { connection_id: connection_id.clone(), form: RestoreForm::from_options(options) },
            TaskKind::SchemaCompare { config } => match serde_json::from_value::<CompareTaskConfig>(config.clone()) {
                Ok(c) => KindDraft::SchemaCompare(CompareDraft::from_config(&c)),
                Err(_) => KindDraft::External(kind.clone()),
            },
            TaskKind::Composite { steps, stop_on_error, parallel } => KindDraft::Composite { steps: steps.clone(), stop_on_error: *stop_on_error, parallel: *parallel },
            TaskKind::Export { .. } | TaskKind::Import { .. } | TaskKind::Migration { .. } => KindDraft::External(kind.clone()),
        }
    }

    /// The task kind, or what is missing.
    pub fn to_kind(&self) -> Result<TaskKind, String> {
        let need_conn = |c: &str| if c.trim().is_empty() { Err(tr("Choose a connection").to_string()) } else { Ok(()) };
        Ok(match self {
            KindDraft::SqlScript(d) => {
                need_conn(&d.connection_id)?;
                let (sql, script_path) = if d.from_file {
                    let p = opt_path(&d.script_path).ok_or_else(|| tr("Choose the script file").to_string())?;
                    (None, Some(p))
                } else {
                    if d.sql.trim().is_empty() {
                        return Err(tr("Enter the SQL to run").to_string());
                    }
                    (Some(d.sql.clone()), None)
                };
                TaskKind::SqlScript {
                    connection_id: d.connection_id.clone(),
                    database: opt(&d.database),
                    schema: opt(&d.schema),
                    sql,
                    script_path,
                    stop_on_error: d.stop_on_error,
                    commit_at_end: d.commit_at_end,
                }
            }
            KindDraft::Shell { command } => {
                if command.trim().is_empty() {
                    return Err(tr("Enter the command to run").to_string());
                }
                TaskKind::Shell { command: command.trim().to_string() }
            }
            KindDraft::Backup { connection_id, form } => {
                need_conn(connection_id)?;
                form.validate()?;
                TaskKind::Backup { connection_id: connection_id.clone(), options: form.to_options() }
            }
            KindDraft::Restore { connection_id, form } => {
                need_conn(connection_id)?;
                form.validate()?;
                TaskKind::Restore { connection_id: connection_id.clone(), options: form.to_options() }
            }
            KindDraft::SchemaCompare(d) => {
                if d.source_connection.trim().is_empty() || d.target_connection.trim().is_empty() {
                    return Err(tr("Choose the source and target connections").to_string());
                }
                let config = serde_json::to_value(d.to_config()).map_err(|e| e.to_string())?;
                TaskKind::SchemaCompare { config }
            }
            KindDraft::Composite { steps, stop_on_error, parallel } => {
                if steps.is_empty() {
                    return Err(tr("A composite task needs at least one step").to_string());
                }
                TaskKind::Composite { steps: steps.clone(), stop_on_error: *stop_on_error, parallel: *parallel }
            }
            KindDraft::External(k) => k.clone(),
        })
    }

    /// Connections used by the form.
    pub fn connection_ids(&self) -> Vec<&str> {
        match self {
            KindDraft::SqlScript(d) => vec![d.connection_id.as_str()],
            KindDraft::Backup { connection_id, .. } | KindDraft::Restore { connection_id, .. } => vec![connection_id.as_str()],
            KindDraft::SchemaCompare(d) => vec![d.source_connection.as_str(), d.target_connection.as_str()],
            KindDraft::External(k) => k.connection_ids(),
            KindDraft::Shell { .. } | KindDraft::Composite { .. } => Vec::new(),
        }
    }
}

/// The task editor's state.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskDraft {
    /// The task being edited (`None` = new task).
    pub original: Option<TaskDef>,
    pub name: String,
    pub folder: String,
    pub description: String,
    pub enabled: bool,
    pub kind: KindDraft,
    pub schedule: ScheduleDraft,
    /// Cancel runs after `timeout_secs`.
    pub timeout: bool,
    pub timeout_secs: u64,
    pub notify_success: bool,
    pub notify_failure: bool,
    /// Also register with the operating system scheduler.
    pub os_scheduler: bool,
}

impl TaskDraft {
    /// Form of a new task.
    pub fn new(kind: NewTaskKind, connection: Option<String>, folder: Option<String>) -> Self {
        let notify = NotifyOptions::default();
        TaskDraft {
            original: None,
            name: kind.label().to_string(),
            folder: folder.unwrap_or_default(),
            description: String::new(),
            enabled: true,
            kind: KindDraft::new(kind, connection),
            schedule: ScheduleDraft::default(),
            timeout: false,
            timeout_secs: 3600,
            notify_success: notify.on_success,
            notify_failure: notify.on_failure,
            os_scheduler: false,
        }
    }

    /// Form editing `task`.
    pub fn from_task(task: &TaskDef) -> Self {
        TaskDraft {
            original: Some(task.clone()),
            name: task.name.clone(),
            folder: task.folder.clone().unwrap_or_default(),
            description: task.description.clone().unwrap_or_default(),
            enabled: task.enabled,
            kind: KindDraft::from_kind(&task.kind),
            schedule: ScheduleDraft::from_schedule(task.schedule.as_ref()),
            timeout: task.timeout_secs.is_some_and(|s| s > 0),
            timeout_secs: task.timeout_secs.filter(|s| *s > 0).unwrap_or(3600),
            notify_success: task.notify.on_success,
            notify_failure: task.notify.on_failure,
            os_scheduler: task.os_scheduler,
        }
    }

    /// Editing an existing task.
    pub fn is_new(&self) -> bool {
        self.original.is_none()
    }

    /// The task definition, or the first problem to fix.
    pub fn to_task(&self) -> Result<TaskDef, String> {
        if self.name.trim().is_empty() {
            return Err(tr("The task needs a name").to_string());
        }
        let kind = self.kind.to_kind()?;
        let schedule = self.schedule.to_schedule()?;
        let mut t = match &self.original {
            Some(o) => o.clone(),
            None => TaskDef::new(self.name.trim(), kind.clone()),
        };
        t.name = self.name.trim().to_string();
        let folder = self.folder.split('/').map(str::trim).filter(|p| !p.is_empty()).collect::<Vec<_>>().join("/");
        t.folder = Some(folder).filter(|f| !f.is_empty());
        t.description = opt(&self.description);
        t.enabled = self.enabled;
        t.kind = kind;
        t.os_scheduler = self.os_scheduler && schedule.is_some();
        t.schedule = schedule;
        t.timeout_secs = self.timeout.then_some(self.timeout_secs.max(1));
        t.notify = NotifyOptions { on_success: self.notify_success, on_failure: self.notify_failure };
        t.validate().map_err(|e| e.message)?;
        Ok(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    #[test]
    fn schedules_roundtrip() {
        for s in [
            Schedule::Cron("30 9 * * 1-5".into()),
            Schedule::Interval { every_secs: 900 },
            Schedule::Interval { every_secs: 7200 },
            Schedule::Interval { every_secs: 45 },
            Schedule::Daily { at: "07:15".into(), weekdays: vec![1, 3, 5] },
            Schedule::Daily { at: "23:00".into(), weekdays: vec![] },
        ] {
            let d = ScheduleDraft::from_schedule(Some(&s));
            assert_eq!(d.to_schedule().unwrap(), Some(s));
        }
        let d = ScheduleDraft::from_schedule(Some(&Schedule::Daily { at: "10:00".into(), weekdays: vec![0] }));
        assert_eq!(d.to_schedule().unwrap(), Some(Schedule::Daily { at: "10:00".into(), weekdays: vec![7] }));
        assert_eq!(ScheduleDraft::from_schedule(None).to_schedule().unwrap(), None);
        let once = ScheduleDraft { mode: ScheduleMode::Once, once_at: "2031-02-03 04:05".into(), ..Default::default() };
        let Some(Schedule::Once { at }) = once.to_schedule().unwrap() else { panic!("once") };
        assert_eq!((at.hour(), at.minute()), (4, 5));
        assert!(parse_local_time("tomorrow").is_err());
    }

    #[test]
    fn task_drafts_roundtrip() {
        let mut d = TaskDraft::new(NewTaskKind::SqlScript, Some("c1".into()), Some("ops/ nightly".into()));
        assert!(d.to_task().is_err(), "SQL missing");
        if let KindDraft::SqlScript(s) = &mut d.kind {
            s.sql = "select 1".into();
        }
        d.schedule = ScheduleDraft { mode: ScheduleMode::Interval, every: 5, unit: IntervalUnit::Minutes, ..Default::default() };
        d.timeout = true;
        d.timeout_secs = 90;
        d.os_scheduler = true;
        let t = d.to_task().unwrap();
        assert_eq!(t.folder.as_deref(), Some("ops/nightly"));
        assert_eq!(t.schedule, Some(Schedule::Interval { every_secs: 300 }));
        assert_eq!(t.timeout_secs, Some(90));
        assert!(t.os_scheduler);
        let back = TaskDraft::from_task(&t);
        assert_eq!(back.to_task().unwrap().kind, t.kind);
        // without a schedule the system scheduler option is dropped
        let mut manual = back.clone();
        manual.schedule.mode = ScheduleMode::None;
        assert!(!manual.to_task().unwrap().os_scheduler);

        let mut b = TaskDraft::new(NewTaskKind::Backup, Some("c1".into()), None);
        assert!(b.to_task().is_err(), "output missing");
        if let KindDraft::Backup { form, .. } = &mut b.kind {
            form.output = "/tmp/b.sql".into();
        }
        let bt = b.to_task().unwrap();
        assert_eq!(TaskDraft::from_task(&bt).to_task().unwrap().kind, bt.kind);

        let mut c = TaskDraft::new(NewTaskKind::SchemaCompare, Some("a".into()), None);
        if let KindDraft::SchemaCompare(cd) = &mut c.kind {
            cd.target_connection = "b".into();
            cd.source_schema = "public".into();
            cd.script_path = "/tmp/m.sql".into();
        }
        let ct = c.to_task().unwrap();
        let TaskKind::SchemaCompare { config } = &ct.kind else { panic!("compare") };
        let cfg: CompareTaskConfig = serde_json::from_value(config.clone()).unwrap();
        assert_eq!((cfg.source_connection.as_str(), cfg.target_connection.as_str()), ("a", "b"));
        assert_eq!(cfg.source_schema.as_deref(), Some("public"));
        assert_eq!(cfg.target_schema, None);
        assert_eq!(cfg.script_path, Some(PathBuf::from("/tmp/m.sql")));

        let ext = TaskKind::Export { connection_id: "c".into(), config: serde_json::json!({"format": "csv"}) };
        assert_eq!(KindDraft::from_kind(&ext).to_kind().unwrap(), ext);
        assert!(TaskDraft::new(NewTaskKind::Composite, None, None).to_task().is_err());
    }
}
