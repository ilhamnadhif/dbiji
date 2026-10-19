//! Application log: every `tracing` event at info level and above is kept in
//! a bounded in-memory buffer (the Error log view) and appended to
//! `logs/dbiji.log` (rotated at 2 MB, mode 0600). Secrets are never logged by
//! the application; the log contains no credentials.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

/// Entries kept in memory.
const CAPACITY: usize = 5000;
/// Size at which the log file is rotated (`dbiji.log` → `dbiji.1.log`).
const MAX_FILE: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct LogEntry {
    pub time: chrono::DateTime<chrono::Local>,
    pub level: Level,
    pub target: String,
    pub message: String,
}

impl LogEntry {
    /// One line of the log file.
    pub fn line(&self) -> String {
        format!("{} {:<5} {} {}", self.time.format("%Y-%m-%d %H:%M:%S%.3f"), self.level, self.target, self.message)
    }
}

struct State {
    entries: VecDeque<LogEntry>,
    /// Increases with every entry (lets views redraw only on change).
    version: u64,
    file: Option<(PathBuf, std::fs::File)>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State { entries: VecDeque::new(), version: 0, file: None }));

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Record an entry (also used for messages that are not `tracing` events).
pub fn push(level: Level, target: &str, message: impl Into<String>) {
    let entry = LogEntry { time: chrono::Local::now(), level, target: target.to_string(), message: message.into() };
    let mut st = state();
    if let Some((_, f)) = &mut st.file {
        let _ = writeln!(f, "{}", entry.line());
    }
    if st.entries.len() == CAPACITY {
        st.entries.pop_front();
    }
    st.entries.push_back(entry);
    st.version += 1;
}

/// The entries in memory, oldest first.
pub fn entries() -> Vec<LogEntry> {
    state().entries.iter().cloned().collect()
}

pub fn version() -> u64 {
    state().version
}

/// Forget the entries in memory (the file is kept).
pub fn clear() {
    let mut st = state();
    st.entries.clear();
    st.version += 1;
}

/// The current log file, once [`open_file`] succeeded.
pub fn log_file() -> Option<PathBuf> {
    state().file.as_ref().map(|(p, _)| p.clone())
}

/// Append the log to `<dir>/dbiji.log` from now on.
pub fn open_file(dir: &Path) -> std::io::Result<PathBuf> {
    dbiji_config::paths::ensure_private_dir(dir).map_err(|e| std::io::Error::other(e.to_string()))?;
    let path = dir.join("dbiji.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_FILE) {
        let _ = std::fs::rename(&path, dir.join("dbiji.1.log"));
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let file = opts.open(&path)?;
    state().file = Some((path.clone(), file));
    Ok(path)
}

/// `tracing` layer feeding the log.
pub struct LogLayer;

struct MessageVisitor(String);

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0.insert_str(0, &format!("{value:?}"));
        } else {
            self.0.push_str(&format!(" {}={value:?}", field.name()));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0.insert_str(0, value);
        } else {
            self.0.push_str(&format!(" {}={value}", field.name()));
        }
    }
}

impl<S: Subscriber> Layer<S> for LogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        if *meta.level() > Level::INFO {
            return;
        }
        let mut v = MessageVisitor(String::new());
        event.record(&mut v);
        push(*meta.level(), meta.target(), v.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_and_file() {
        let dir = std::env::temp_dir().join(format!("dbiji-log-{}", uuid::Uuid::new_v4().simple()));
        let path = open_file(&dir).unwrap();
        push(Level::ERROR, "dbiji::test", "something failed");
        push(Level::WARN, "dbiji::test", "careful");
        let e = entries();
        assert!(e.iter().any(|e| e.level == Level::ERROR && e.message == "something failed"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("ERROR dbiji::test something failed"), "{text}");
        #[cfg(unix)]
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&path).unwrap().permissions()) & 0o777, 0o600);
        let v = version();
        clear();
        assert!(entries().is_empty() && version() > v);
        state().file = None;
        std::fs::remove_dir_all(&dir).ok();
    }
}
