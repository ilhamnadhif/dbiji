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
    /// Problems found while loading the configuration (shown once).
    pub warnings: Vec<String>,
}

impl Services {
    /// Load everything from `paths`. Never fails: problems become warnings.
    pub fn load(paths: AppPaths, settings: &Settings) -> Services {
        let mut warnings = Vec::new();
        if let Err(e) = paths.ensure_dirs() {
            warnings.push(e.to_string());
        }
        let secrets = Arc::new(RwLock::new(SecretsManager::new(&paths, &settings.security)));
        let history = if settings.history.enabled {
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
        } else {
            None
        };
        let projects = match ProjectManager::new(&paths) {
            Ok(p) => Some(p),
            Err(e) => {
                warnings.push(e.to_string());
                None
            }
        };
        let (templates, template_warnings) = TemplateLibrary::load(&paths);
        warnings.extend(template_warnings);
        Services { paths, secrets, history, projects, templates, warnings }
    }

    /// In-memory services for tests and benchmarks (nothing touches the
    /// user's configuration, Keychain or history).
    pub fn ephemeral(root: &std::path::Path) -> Services {
        let paths = AppPaths::with_root(root);
        let secrets = Arc::new(RwLock::new(SecretsManager::with_store(
            dbiji_config::SecretBackend::Keychain,
            Arc::new(dbiji_config::MemoryStore::new()),
        )));
        let history = HistoryStore::open_in_memory().ok().map(|h| Arc::new(Mutex::new(h)));
        Services {
            projects: ProjectManager::new(&paths).ok(),
            templates: TemplateLibrary::default(),
            paths,
            secrets,
            history,
            warnings: Vec::new(),
        }
    }

    /// Record an executed statement in the history (runs on a worker thread).
    pub fn history_sink(&self, connection_id: Option<String>, connection_name: Option<String>, database: Option<String>) -> Option<crate::exec::HistorySink> {
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
            e.duration_ms = Some(r.duration.as_millis() as u64);
            e.rows = r.rows;
            e.error_message = r.error.clone();
            if let Err(err) = history.lock().add(&e) {
                tracing::warn!("cannot write query history: {err}");
            }
        }))
    }
}
