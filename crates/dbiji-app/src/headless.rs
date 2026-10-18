//! Headless entry points (`dbiji --run-task <id|name>`).

use crate::backend::Backend;
use crate::services::Settings;
use dbiji_config::AppPaths;
use std::process::ExitCode;

/// Run a saved task without opening a window and exit with its status.
pub fn run_task(backend: &Backend, paths: &AppPaths, settings: &Settings, task: &str) -> ExitCode {
    let _ = (backend, paths, settings);
    eprintln!("task runner is not available in this build: {task}");
    ExitCode::FAILURE
}
