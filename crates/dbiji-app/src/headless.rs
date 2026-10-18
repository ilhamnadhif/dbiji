//! Headless entry points (`dbiji --run-task <id|name>`), also used by the
//! operating system scheduler (launchd / cron / Task Scheduler).

use crate::backend::Backend;
use crate::services::Settings;
use dbiji_config::{AppPaths, SecretBackend, SecretsManager};
use dbiji_core::Secret;
use dbiji_tasks::{RunLog, RunLogOptions, TaskEvent, TaskStore, Trigger};
use std::process::ExitCode;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Environment variable holding the vault master password for headless runs
/// when secrets are kept in the encrypted file.
pub const VAULT_PASSWORD_ENV: &str = "DBIJI_VAULT_PASSWORD";

/// Task executor over the user's saved connections and secrets.
pub fn executor(paths: &AppPaths, secrets: Arc<parking_lot::RwLock<SecretsManager>>) -> Arc<crate::taskexec::AppTaskExecutor> {
    Arc::new(crate::taskexec::AppTaskExecutor {
        paths: paths.clone(),
        registry: Arc::new(crate::driver_registry()),
        secrets: Arc::new(crate::conn::ManagedSecrets(secrets)),
        net: Arc::new(crate::conn::TunnelNet { known_hosts: paths.known_hosts_file() }),
    })
}

/// Run a saved task without opening a window and exit with its status
/// (0 succeeded, 1 failed, 2 cancelled, 3 skipped, 4 setup error).
pub fn run_task(backend: &Backend, paths: &AppPaths, settings: &Settings, task: &str) -> ExitCode {
    let secrets = SecretsManager::new(paths, &settings.security);
    if settings.security.secret_backend == SecretBackend::EncryptedFile
        && let (Some(v), Ok(pw)) = (secrets.vault(), std::env::var(VAULT_PASSWORD_ENV))
            && let Err(e) = v.unlock(&Secret::new(pw)) {
                eprintln!("cannot unlock the vault: {e}");
                return ExitCode::from(4);
            }
    let secrets = Arc::new(parking_lot::RwLock::new(secrets));
    let (store, log) = match (TaskStore::open(paths.tasks_dir()), RunLog::open(paths.tasks_dir(), RunLogOptions::default())) {
        (Ok(s), Ok(l)) => (Arc::new(s), Arc::new(l)),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("cannot open the task store: {e}");
            return ExitCode::from(4);
        }
    };
    let exec = executor(paths, secrets);
    let cancel = CancellationToken::new();
    let on_event: dbiji_tasks::EventCallback = Box::new(|ev: &TaskEvent| match ev {
        TaskEvent::Started { run } => println!("started {} ({})", run.task_name, run.run_id),
        TaskEvent::Message { message, .. } => println!("{:?}: {}", message.level, message.text),
        TaskEvent::Progress { progress, .. } => {
            if let Some(t) = progress.total {
                println!("progress {}/{}", progress.done, t);
            }
        }
        TaskEvent::Finished { run } => println!("finished: {:?}", run.status),
    });
    let c2 = cancel.clone();
    let result = backend.block_on(async move {
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                c2.cancel();
            }
        });
        dbiji_tasks::run_task_once(store, log, exec, task, Trigger::Cli, Some(on_event), cancel).await
    });
    match result {
        Ok(run) => ExitCode::from(run.exit_code().clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("{}", e.full_text());
            ExitCode::from(4)
        }
    }
}
