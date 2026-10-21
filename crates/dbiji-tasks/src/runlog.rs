//! Execution log of task runs (JSON lines with rotation).
//!
//! Every run is written when it starts (status `running`) and again when it
//! finishes; readers merge records by `run_id`, the last one wins. When the
//! current file exceeds [`RunLogOptions::max_file_bytes`] it is rotated
//! (`runs.jsonl` -> `runs.1.jsonl` -> ... up to [`RunLogOptions::max_files`]).

use crate::model::{MessageLevel, RunMessage, RunStatus, TaskRun};
use chrono::Utc;
use dbiji_core::{DbError, DbResult, tr};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// Size limits of the run log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunLogOptions {
    /// Rotate the current file when it grows beyond this size.
    pub max_file_bytes: u64,
    /// Number of files kept, the current one included.
    pub max_files: usize,
}

impl Default for RunLogOptions {
    fn default() -> Self {
        RunLogOptions { max_file_bytes: 1024 * 1024, max_files: 5 }
    }
}

/// Append-only run log in a directory.
#[derive(Debug)]
pub struct RunLog {
    dir: PathBuf,
    opts: RunLogOptions,
    lock: Mutex<()>,
}

fn io_err(what: &str, path: &Path, e: std::io::Error) -> DbError {
    DbError::io(format!("{what} {}: {e}", path.display()))
}

impl RunLog {
    /// Name of the current log file.
    pub const FILE_NAME: &'static str = "runs.jsonl";

    /// Open (or create) the log in `dir`.
    pub fn open(dir: impl AsRef<Path>, opts: RunLogOptions) -> DbResult<RunLog> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir).map_err(|e| io_err("Cannot create", &dir, e))?;
        Ok(RunLog { dir, opts: RunLogOptions { max_files: opts.max_files.max(1), ..opts }, lock: Mutex::new(()) })
    }

    fn file(&self, n: usize) -> PathBuf {
        if n == 0 { self.dir.join(Self::FILE_NAME) } else { self.dir.join(format!("runs.{n}.jsonl")) }
    }

    /// Existing log files, oldest first.
    pub fn files(&self) -> Vec<PathBuf> {
        (0..self.opts.max_files).rev().map(|n| self.file(n)).filter(|p| p.exists()).collect()
    }

    /// Append a record of `run` (a start or a final state).
    pub fn record(&self, run: &TaskRun) -> DbResult<()> {
        let mut line = serde_json::to_string(run).map_err(|e| DbError::internal(e.to_string()))?;
        line.push('\n');
        let _g = self.lock.lock();
        let path = self.file(0);
        {
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path).map_err(|e| io_err("Cannot write", &path, e))?;
            // one write per record keeps concurrent appenders (CLI runs) line-atomic
            f.write_all(line.as_bytes()).map_err(|e| io_err("Cannot write", &path, e))?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if size > self.opts.max_file_bytes {
            self.rotate()?;
        }
        Ok(())
    }

    fn rotate(&self) -> DbResult<()> {
        let max = self.opts.max_files;
        if max <= 1 {
            let p = self.file(0);
            return std::fs::remove_file(&p).map_err(|e| io_err("Cannot rotate", &p, e));
        }
        let oldest = self.file(max - 1);
        if oldest.exists() {
            std::fs::remove_file(&oldest).map_err(|e| io_err("Cannot rotate", &oldest, e))?;
        }
        for n in (0..max - 1).rev() {
            let from = self.file(n);
            if from.exists() {
                let to = self.file(n + 1);
                std::fs::rename(&from, &to).map_err(|e| io_err("Cannot rotate", &from, e))?;
            }
        }
        Ok(())
    }

    /// Every run (merged), newest first.
    pub fn runs(&self) -> DbResult<Vec<TaskRun>> {
        let _g = self.lock.lock();
        let mut order: Vec<String> = Vec::new();
        let mut by_id: HashMap<String, TaskRun> = HashMap::new();
        for path in self.files() {
            let f = match std::fs::File::open(&path) {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(io_err("Cannot read", &path, e)),
            };
            for line in BufReader::new(f).lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::warn!("run log {}: unreadable line: {e}", path.display());
                        continue;
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<TaskRun>(&line) {
                    Ok(run) => {
                        if !by_id.contains_key(&run.run_id) {
                            order.push(run.run_id.clone());
                        }
                        by_id.insert(run.run_id.clone(), run);
                    }
                    Err(e) => tracing::warn!("run log {}: skipping malformed record: {e}", path.display()),
                }
            }
        }
        let mut runs: Vec<TaskRun> = order.into_iter().filter_map(|id| by_id.remove(&id)).collect();
        runs.sort_by(|a, b| b.started.cmp(&a.started).then_with(|| b.run_id.cmp(&a.run_id)));
        Ok(runs)
    }

    /// The `limit` most recent runs.
    pub fn recent(&self, limit: usize) -> DbResult<Vec<TaskRun>> {
        let mut r = self.runs()?;
        r.truncate(limit);
        Ok(r)
    }

    /// Runs of one task, newest first.
    pub fn runs_for_task(&self, task_id: &str, limit: usize) -> DbResult<Vec<TaskRun>> {
        Ok(self.runs()?.into_iter().filter(|r| r.task_id == task_id).take(limit).collect())
    }

    /// A run by id.
    pub fn get(&self, run_id: &str) -> DbResult<Option<TaskRun>> {
        Ok(self.runs()?.into_iter().find(|r| r.run_id == run_id))
    }

    /// Most recent run of a task.
    pub fn latest(&self, task_id: &str) -> DbResult<Option<TaskRun>> {
        Ok(self.runs_for_task(task_id, 1)?.into_iter().next())
    }

    /// Status of the most recent run of a task.
    pub fn latest_status(&self, task_id: &str) -> DbResult<Option<RunStatus>> {
        Ok(self.latest(task_id)?.map(|r| r.status))
    }

    /// Most recent run of every task (top level runs and composite steps alike).
    pub fn latest_by_task(&self) -> DbResult<HashMap<String, TaskRun>> {
        let mut m = HashMap::new();
        for r in self.runs()? {
            m.entry(r.task_id.clone()).or_insert(r);
        }
        Ok(m)
    }

    /// Close runs left in `running` state by another (exited) process: they
    /// are recorded as failed. Returns the number of runs closed.
    pub fn mark_interrupted(&self, current_pid: u32) -> DbResult<usize> {
        // A run of a process that still lives (a `--run-task` started by the OS scheduler) is not interrupted.
        let stale: Vec<TaskRun> = self.runs()?.into_iter().filter(|r| r.status == RunStatus::Running && r.pid != current_pid && !process_alive(r.pid)).collect();
        for mut r in stale.iter().cloned() {
            let now = Utc::now();
            r.status = RunStatus::Failed;
            r.finished = Some(now);
            let text = tr("Interrupted: DBiji exited while the task was running").to_string();
            r.error = Some(text.clone());
            r.push_message(RunMessage { time: now, level: MessageLevel::Error, text });
            self.record(&r)?;
        }
        Ok(stale.len())
    }

    /// Delete all log files.
    pub fn clear(&self) -> DbResult<()> {
        let _g = self.lock.lock();
        for p in self.files() {
            std::fs::remove_file(&p).map_err(|e| io_err("Cannot delete", &p, e))?;
        }
        Ok(())
    }
}

/// The process `pid` is running (unknown on Windows: treated as exited).
fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        if pid == 0 || pid > i32::MAX as u32 {
            return false;
        }
        // Signal 0 only checks that the process exists (EPERM: it exists, owned by another user).
        // SAFETY: kill(2) with signal 0 sends nothing.
        let r = unsafe { libc::kill(pid as i32, 0) };
        r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{TaskDef, TaskKind, Trigger};
    use chrono::Duration;

    fn task(name: &str) -> TaskDef {
        TaskDef::new(name, TaskKind::Shell { command: "true".into() })
    }

    #[test]
    fn start_and_finish_records_merge() {
        let dir = tempfile::tempdir().unwrap();
        let log = RunLog::open(dir.path(), RunLogOptions::default()).unwrap();
        let t = task("a");
        let t0 = Utc::now();
        let mut r = TaskRun::start(&t, Trigger::Manual, t0, None);
        log.record(&r).unwrap();
        assert_eq!(log.latest_status(&t.id).unwrap(), Some(RunStatus::Running));
        r.status = RunStatus::Succeeded;
        r.finished = Some(t0 + Duration::seconds(3));
        r.rows = Some(42);
        log.record(&r).unwrap();
        let runs = log.runs().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].rows, Some(42));
        assert_eq!(runs[0].duration(), Some(Duration::seconds(3)));
        let mut r2 = TaskRun::start(&t, Trigger::Schedule, t0 + Duration::seconds(10), None);
        r2.status = RunStatus::Failed;
        log.record(&r2).unwrap();
        let other = TaskRun::start(&task("b"), Trigger::Cli, t0 + Duration::seconds(5), None);
        log.record(&other).unwrap();
        assert_eq!(log.runs_for_task(&t.id, 10).unwrap().len(), 2);
        assert_eq!(log.latest_status(&t.id).unwrap(), Some(RunStatus::Failed));
        assert_eq!(log.recent(1).unwrap()[0].run_id, r2.run_id);
        assert_eq!(log.latest_by_task().unwrap().len(), 2);
        assert_eq!(log.get(&r.run_id).unwrap().unwrap().status, RunStatus::Succeeded);
        // malformed lines are skipped
        let mut f = std::fs::OpenOptions::new().append(true).open(dir.path().join(RunLog::FILE_NAME)).unwrap();
        f.write_all(b"garbage\n\n").unwrap();
        assert_eq!(log.runs().unwrap().len(), 3);
        log.clear().unwrap();
        assert!(log.runs().unwrap().is_empty());
    }

    #[test]
    fn rotation_by_size_and_count() {
        let dir = tempfile::tempdir().unwrap();
        let log = RunLog::open(dir.path(), RunLogOptions { max_file_bytes: 2_000, max_files: 3 }).unwrap();
        let t = task("rotating");
        let base = Utc::now();
        let mut ids = Vec::new();
        for i in 0..60 {
            let mut r = TaskRun::start(&t, Trigger::Schedule, base + Duration::seconds(i), None);
            r.status = RunStatus::Succeeded;
            r.error = Some("x".repeat(100));
            ids.push(r.run_id.clone());
            log.record(&r).unwrap();
        }
        let files = log.files();
        // the current file is recreated by the next write after a rotation
        assert!(files.len() == 2 || files.len() == 3, "{files:?}");
        assert!(dir.path().join("runs.2.jsonl").exists());
        assert!(!dir.path().join("runs.3.jsonl").exists());
        for f in &files {
            assert!(std::fs::metadata(f).unwrap().len() <= 2_000 + 400);
        }
        let runs = log.runs().unwrap();
        assert!(runs.len() < 60 && runs.len() >= 5, "{}", runs.len());
        // newest kept, oldest dropped
        assert_eq!(runs[0].run_id, *ids.last().unwrap());
        assert!(!runs.iter().any(|r| r.run_id == ids[0]));
        // a run whose start and finish records are split across files still merges
        let mut r = TaskRun::start(&t, Trigger::Manual, base + Duration::seconds(100), None);
        log.record(&r).unwrap();
        for i in 0..20 {
            let mut x = TaskRun::start(&t, Trigger::Schedule, base + Duration::seconds(101 + i), None);
            x.error = Some("y".repeat(100));
            log.record(&x).unwrap();
        }
        r.status = RunStatus::Succeeded;
        log.record(&r).unwrap();
        assert_eq!(log.get(&r.run_id).unwrap().unwrap().status, RunStatus::Succeeded);
    }

    #[test]
    fn interrupted_runs_are_closed() {
        let dir = tempfile::tempdir().unwrap();
        let log = RunLog::open(dir.path(), RunLogOptions::default()).unwrap();
        let mut stale = TaskRun::start(&task("a"), Trigger::Schedule, Utc::now(), None);
        // the pid of a process that has exited (a live one is left running)
        stale.pid = if cfg!(unix) {
            let mut child = std::process::Command::new("true").spawn().unwrap();
            child.wait().unwrap();
            child.id()
        } else {
            std::process::id().wrapping_add(1)
        };
        log.record(&stale).unwrap();
        let mine = TaskRun::start(&task("b"), Trigger::Manual, Utc::now(), None);
        log.record(&mine).unwrap();
        assert_eq!(log.mark_interrupted(std::process::id()).unwrap(), 1);
        assert_eq!(log.get(&stale.run_id).unwrap().unwrap().status, RunStatus::Failed);
        assert_eq!(log.get(&mine.run_id).unwrap().unwrap().status, RunStatus::Running);
        assert_eq!(log.mark_interrupted(std::process::id()).unwrap(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn runs_of_live_processes_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let log = RunLog::open(dir.path(), RunLogOptions::default()).unwrap();
        let mut child = std::process::Command::new("sleep").arg("5").spawn().unwrap();
        let mut other = TaskRun::start(&task("a"), Trigger::Schedule, Utc::now(), None);
        other.pid = child.id();
        log.record(&other).unwrap();
        assert_eq!(log.mark_interrupted(std::process::id()).unwrap(), 0);
        let _ = child.kill();
        let _ = child.wait();
    }
}
