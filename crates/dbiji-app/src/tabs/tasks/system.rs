//! Registering tasks with the operating system scheduler so that they also
//! run while DBiji is closed: the entry runs `<current exe> --run-task <id>`
//! (a launchd agent on macOS, a crontab line on Linux and other Unix
//! systems, a Task Scheduler entry on Windows). [`sync`] and [`remove`] are
//! only called from explicit user actions (saving, enabling, disabling or
//! deleting a task that has the option).

use dbiji_core::{DbError, DbResult, tr, trf};
use dbiji_tasks::TaskDef;
#[allow(unused_imports)]
use dbiji_tasks::os::{self, OsCommand};
use std::path::{Path, PathBuf};

/// Name of the operating system scheduler.
pub fn scheduler_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "launchd"
    } else if cfg!(windows) {
        tr("Windows Task Scheduler")
    } else {
        "cron"
    }
}

/// The task should have an operating system entry.
pub fn wanted(task: &TaskDef) -> bool {
    task.os_scheduler && task.enabled && task.schedule.is_some()
}

/// The DBiji executable the entry runs.
pub fn current_exe() -> DbResult<PathBuf> {
    std::env::current_exe().map_err(|e| DbError::io(trf("Cannot find the DBiji program: {}", &[&e])))
}

/// Text of the entry for `task` (launchd property list, crontab line or
/// schtasks command). Fails when the schedule cannot be expressed.
pub fn entry_preview(task: &TaskDef, exe: &Path) -> DbResult<String> {
    #[cfg(target_os = "macos")]
    {
        os::launchd_plist(task, exe)
    }
    #[cfg(windows)]
    {
        os::schtasks_create(task, exe).map(|c| c.display())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        os::crontab_line(task, exe)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (task, exe);
        Err(DbError::unsupported(tr("The system scheduler is not supported on this platform")))
    }
}

/// What [`sync`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Synced {
    /// The entry was written (path or command).
    Installed(String),
    /// The entry was removed (`false`: there was none).
    Removed(bool),
}

#[allow(dead_code)]
async fn run(cmd: OsCommand, stdin: Option<String>) -> DbResult<String> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;
    let mut c = tokio::process::Command::new(&cmd.program);
    c.args(&cmd.args).kill_on_drop(true).stdout(Stdio::piped()).stderr(Stdio::piped());
    c.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = c.spawn().map_err(|e| DbError::io(trf("Cannot start {}: {}", &[&cmd.program, &e])))?;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(input.as_bytes()).await?;
        pipe.shutdown().await?;
        drop(pipe);
    }
    let out = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait_with_output())
        .await
        .map_err(|_| DbError::timeout(trf("{} did not finish", &[&cmd.program])))??;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(DbError::io(trf("{} failed: {}", &[&cmd.display(), &if err.is_empty() { out.status.to_string() } else { err }])))
    }
}

#[cfg(target_os = "macos")]
fn agents_dir() -> DbResult<PathBuf> {
    os::default_launch_agents_dir().ok_or_else(|| DbError::config(tr("Cannot find the LaunchAgents folder")))
}

/// Install, update or remove the entry of `task` so that it matches
/// [`wanted`].
#[cfg(target_os = "macos")]
pub async fn sync(task: TaskDef) -> DbResult<Synced> {
    let dir = agents_dir()?;
    let file = os::launch_agent_file(&dir, &task.id);
    if file.exists() {
        // unload the previous version (the schedule may have changed)
        let _ = run(os::launchctl_unload(&file), None).await;
    }
    if !wanted(&task) {
        return os::uninstall_launch_agent(&task.id, &dir).map(Synced::Removed);
    }
    let exe = current_exe()?;
    let path = os::install_launch_agent(&task, &exe, &dir)?;
    run(os::launchctl_load(&path), None).await?;
    Ok(Synced::Installed(path.display().to_string()))
}

/// Remove the entry of a task (returns false when there was none).
#[cfg(target_os = "macos")]
pub async fn remove(task_id: String) -> DbResult<bool> {
    let dir = agents_dir()?;
    let file = os::launch_agent_file(&dir, &task_id);
    if file.exists() {
        let _ = run(os::launchctl_unload(&file), None).await;
    }
    os::uninstall_launch_agent(&task_id, &dir)
}

#[cfg(all(unix, not(target_os = "macos")))]
async fn read_crontab() -> DbResult<String> {
    match run(OsCommand { program: "crontab".into(), args: vec!["-l".into()] }, None).await {
        Ok(s) => Ok(s),
        Err(e) if e.message.to_ascii_lowercase().contains("no crontab") => Ok(String::new()),
        Err(e) => Err(e),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
async fn write_crontab(text: String) -> DbResult<()> {
    run(OsCommand { program: "crontab".into(), args: vec!["-".into()] }, Some(text)).await.map(|_| ())
}

/// Install, update or remove the entry of `task` so that it matches
/// [`wanted`].
#[cfg(all(unix, not(target_os = "macos")))]
pub async fn sync(task: TaskDef) -> DbResult<Synced> {
    let existing = read_crontab().await?;
    if wanted(&task) {
        let exe = current_exe()?;
        let updated = os::crontab_with_task(&existing, &task, &exe)?;
        if updated != existing {
            write_crontab(updated).await?;
        }
        Ok(Synced::Installed(os::crontab_line(&task, &exe)?))
    } else {
        let updated = os::crontab_without_task(&existing, &task.id);
        let changed = updated != existing;
        if changed {
            write_crontab(updated).await?;
        }
        Ok(Synced::Removed(changed))
    }
}

/// Remove the entry of a task (returns false when there was none).
#[cfg(all(unix, not(target_os = "macos")))]
pub async fn remove(task_id: String) -> DbResult<bool> {
    let existing = read_crontab().await?;
    let updated = os::crontab_without_task(&existing, &task_id);
    if updated == existing {
        return Ok(false);
    }
    write_crontab(updated).await?;
    Ok(true)
}

/// Install, update or remove the entry of `task` so that it matches
/// [`wanted`].
#[cfg(windows)]
pub async fn sync(task: TaskDef) -> DbResult<Synced> {
    if !wanted(&task) {
        return remove(task.id.clone()).await.map(Synced::Removed);
    }
    let exe = current_exe()?;
    let cmd = os::schtasks_create(&task, &exe)?;
    let shown = cmd.display();
    run(cmd, None).await?;
    Ok(Synced::Installed(shown))
}

/// Remove the entry of a task (returns false when there was none).
#[cfg(windows)]
pub async fn remove(task_id: String) -> DbResult<bool> {
    // schtasks fails when the entry does not exist
    Ok(run(os::schtasks_delete(&task_id), None).await.is_ok())
}

/// Install, update or remove the entry of `task`.
#[cfg(not(any(unix, windows)))]
pub async fn sync(task: TaskDef) -> DbResult<Synced> {
    let _ = task;
    Err(DbError::unsupported(tr("The system scheduler is not supported on this platform")))
}

/// Remove the entry of a task.
#[cfg(not(any(unix, windows)))]
pub async fn remove(task_id: String) -> DbResult<bool> {
    let _ = task_id;
    Ok(false)
}
