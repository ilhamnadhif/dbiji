//! Long-lived application services shared by all UI components: paths,
//! preferences, secrets, query history, projects and SQL templates.

use dbiji_config::{AppPaths, HistoryEntry, HistoryStore, ProjectManager, SecretsManager, TemplateLibrary};
use parking_lot::{Mutex, RwLock};
use std::sync::Arc;

/// Application preferences (`settings.json` + `keymap.json`).
pub type Settings = dbiji_config::AppSettings;

pub struct Services {
    pub paths: AppPaths,
    pub secrets: Arc<RwLock<SecretsManager>>,
    /// `None` when the history database cannot be opened.
    pub history: Option<Arc<Mutex<HistoryStore>>>,
    pub projects: Option<ProjectManager>,
    pub templates: TemplateLibrary,
    /// Saved tasks (FR-A3).
    pub tasks: Option<Arc<dbiji_tasks::TaskStore>>,
    /// Task run history.
    pub runlog: Option<Arc<dbiji_tasks::RunLog>>,
    /// Runs tasks: on their schedule (when enabled in the settings) and on demand.
    pub scheduler: Option<Arc<dbiji_tasks::Scheduler>>,
    /// Problems found while loading the configuration (shown once).
    pub warnings: Vec<String>,
}

impl Services {
    /// Load everything from `paths`. Never fails: problems become warnings.
    pub fn load(paths: AppPaths, settings: &Settings, runtime: tokio::runtime::Handle) -> Services {
        let mut warnings = Vec::new();
        if let Err(e) = paths.ensure_dirs() {
            warnings.push(e.to_string());
        }
        let secrets = Arc::new(RwLock::new(SecretsManager::new(&paths, &settings.security)));
        let history = if settings.history.enabled { Self::open_history(&paths, settings, &mut warnings) } else { None };
        let projects = match ProjectManager::new(&paths) {
            Ok(p) => Some(p),
            Err(e) => {
                warnings.push(e.to_string());
                None
            }
        };
        let (templates, template_warnings) = TemplateLibrary::load(&paths);
        warnings.extend(template_warnings);
        Self::start_auto_lock(&secrets, settings.security.auto_lock_minutes, &runtime);
        let (tasks, runlog, scheduler) = Self::task_services(&paths, &secrets, settings.tasks.scheduler_enabled, runtime, &mut warnings);
        Services { paths, secrets, history, projects, templates, tasks, runlog, scheduler, warnings }
    }

    /// The query history database (`None` when it cannot be opened: a warning).
    pub fn open_history(paths: &AppPaths, settings: &Settings, warnings: &mut Vec<String>) -> Option<Arc<Mutex<HistoryStore>>> {
        match HistoryStore::open_or_recover(&paths.history_db()) {
            Ok((mut h, note)) => {
                if let Some(n) = note {
                    warnings.push(n);
                }
                h.set_redaction(settings.history.redact_secrets);
                let _ = h.set_retention(Some(settings.history.retention()));
                Some(Arc::new(Mutex::new(h)))
            }
            Err(e) => {
                warnings.push(e.to_string());
                None
            }
        }
    }

    /// Lock the encrypted vault after the configured idle time (checked on the
    /// I/O runtime, so it also works while the UI is idle).
    fn start_auto_lock(secrets: &Arc<RwLock<SecretsManager>>, minutes: u32, runtime: &tokio::runtime::Handle) {
        if minutes == 0 {
            return;
        }
        let secrets = Arc::downgrade(secrets);
        runtime.spawn(async move {
            let idle = std::time::Duration::from_secs(minutes as u64 * 60);
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                let Some(s) = secrets.upgrade() else { break };
                let vault = s.read().vault().cloned();
                if let Some(v) = vault
                    && v.lock_if_idle(idle)
                {
                    tracing::info!("vault locked after {minutes} idle minutes");
                }
            }
        });
    }

    #[allow(clippy::type_complexity)]
    fn task_services(
        paths: &AppPaths,
        secrets: &Arc<RwLock<SecretsManager>>,
        schedule: bool,
        runtime: tokio::runtime::Handle,
        warnings: &mut Vec<String>,
    ) -> (Option<Arc<dbiji_tasks::TaskStore>>, Option<Arc<dbiji_tasks::RunLog>>, Option<Arc<dbiji_tasks::Scheduler>>) {
        let store = match dbiji_tasks::TaskStore::open(paths.tasks_dir()) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                warnings.push(e.to_string());
                return (None, None, None);
            }
        };
        let log = match dbiji_tasks::RunLog::open(paths.tasks_dir(), dbiji_tasks::RunLogOptions::default()) {
            Ok(l) => Arc::new(l),
            Err(e) => {
                warnings.push(e.to_string());
                return (Some(store), None, None);
            }
        };
        let exec = crate::headless::executor(paths, secrets.clone());
        let scheduler = Arc::new(dbiji_tasks::Scheduler::new(store.clone(), log.clone(), exec, dbiji_tasks::SchedulerOptions::default(), runtime));
        if schedule && let Err(e) = scheduler.start() {
            warnings.push(e.to_string());
        }
        (Some(store), Some(log), Some(scheduler))
    }

    /// In-memory services for tests and benchmarks (nothing touches the
    /// user's configuration, Keychain or history).
    pub fn ephemeral(root: &std::path::Path) -> Services {
        let paths = AppPaths::with_root(root);
        let secrets = Arc::new(RwLock::new(SecretsManager::with_store(dbiji_config::SecretBackend::Keychain, Arc::new(dbiji_config::MemoryStore::new()))));
        let history = HistoryStore::open_in_memory().ok().map(|h| Arc::new(Mutex::new(h)));
        let tasks = dbiji_tasks::TaskStore::open(paths.tasks_dir()).ok().map(Arc::new);
        let runlog = dbiji_tasks::RunLog::open(paths.tasks_dir(), dbiji_tasks::RunLogOptions::default()).ok().map(Arc::new);
        Services {
            projects: ProjectManager::new(&paths).ok(),
            templates: TemplateLibrary::default(),
            paths,
            secrets,
            history,
            tasks,
            runlog,
            scheduler: None,
            warnings: Vec::new(),
        }
    }

    /// Record an executed statement in the history (runs on a worker thread).
    pub fn history_sink(
        &self,
        connection_id: Option<String>,
        connection_name: Option<String>,
        database: Option<String>,
        schema: Option<String>,
    ) -> Option<crate::exec::HistorySink> {
        let history = self.history.clone()?;
        Some(Arc::new(move |r: crate::exec::HistoryRecord| {
            let status = if r.cancelled {
                dbiji_config::QueryStatus::Cancelled
            } else if r.error.is_some() {
                dbiji_config::QueryStatus::Error
            } else {
                dbiji_config::QueryStatus::Ok
            };
            let mut e = HistoryEntry::new(r.sql.clone(), status);
            e.timestamp_ms = r.started.timestamp_millis();
            e.connection_id = connection_id.clone();
            e.connection_name = connection_name.clone();
            e.database = database.clone();
            e.schema = schema.clone();
            e.duration_ms = Some(r.duration.as_millis() as u64);
            e.rows = r.rows;
            e.error_message = r.error.clone();
            if let Err(err) = history.lock().add(&e) {
                tracing::warn!("cannot write query history: {err}");
            }
        }))
    }
}
