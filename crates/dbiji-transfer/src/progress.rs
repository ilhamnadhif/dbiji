//! Progress reporting shared between a running transfer and the UI.
//!
//! A [`Progress`] is created by the caller (usually wrapped in an `Arc`),
//! handed to the transfer function and polled by the UI. Counters are plain
//! atomics so updating them from the hot loop costs almost nothing; names and
//! messages live behind a short-lived lock.

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Number of messages kept; older ones are dropped (and counted) so a job
/// logging an error per row cannot exhaust memory.
const MAX_MESSAGES: usize = 1000;
const UNKNOWN: u64 = u64::MAX;

/// Severity of a [`ProgressMessage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MessageLevel {
    /// Informational note (e.g. "created table x").
    #[default]
    Info,
    /// Something was adjusted or skipped but the operation continues.
    Warning,
    /// A row or an item failed.
    Error,
}

/// A message logged by a running transfer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProgressMessage {
    /// Severity.
    pub level: MessageLevel,
    /// Item (table, file, query) the message refers to, if any.
    pub item: Option<String>,
    /// Message text.
    pub text: String,
    /// Seconds since the operation started.
    pub elapsed_secs: f64,
}

/// Point-in-time copy of a [`Progress`], convenient for the UI and task logs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProgressSnapshot {
    /// Rows processed so far (all items together).
    pub rows: u64,
    /// Total number of rows when known.
    pub total_rows: Option<u64>,
    /// Bytes written to output files (export) so far.
    pub bytes: u64,
    /// Finished items (tables, queries) of a multi-object job.
    pub items_done: u64,
    /// Total items of a multi-object job.
    pub items_total: u64,
    /// Items currently being processed.
    pub active_items: Vec<String>,
    /// Rows or items that failed.
    pub errors: u64,
    /// Seconds since the operation started.
    pub elapsed_secs: f64,
    /// Average throughput.
    pub rows_per_second: f64,
    /// Completed fraction in `0.0..=1.0` when it can be estimated.
    pub fraction: Option<f64>,
}

#[derive(Debug, Default)]
struct State {
    active_items: Vec<String>,
    messages: VecDeque<ProgressMessage>,
    dropped_messages: u64,
}

/// Live progress of an export, import, migration or mock data run.
///
/// Create one per operation; it is `Sync` and normally shared as
/// `Arc<Progress>` between the worker and the UI.
#[derive(Debug)]
pub struct Progress {
    started: Instant,
    rows: AtomicU64,
    bytes: AtomicU64,
    total_rows: AtomicU64,
    items_done: AtomicU64,
    items_total: AtomicU64,
    errors: AtomicU64,
    state: Mutex<State>,
}

impl Default for Progress {
    fn default() -> Self {
        Self::new()
    }
}

impl Progress {
    /// New progress with all counters at zero; the clock starts now.
    pub fn new() -> Self {
        Progress {
            started: Instant::now(),
            rows: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            total_rows: AtomicU64::new(UNKNOWN),
            items_done: AtomicU64::new(0),
            items_total: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            state: Mutex::new(State::default()),
        }
    }

    /// New progress wrapped in an `Arc` for sharing with the UI.
    pub fn shared() -> Arc<Progress> {
        Arc::new(Self::new())
    }

    /// Rows processed so far.
    pub fn rows(&self) -> u64 {
        self.rows.load(Ordering::Relaxed)
    }

    /// Count `n` more processed rows.
    pub fn add_rows(&self, n: u64) {
        self.rows.fetch_add(n, Ordering::Relaxed);
    }

    /// Bytes written so far.
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Count `n` more written bytes.
    pub fn add_bytes(&self, n: u64) {
        self.bytes.fetch_add(n, Ordering::Relaxed);
    }

    /// Total rows when known.
    pub fn total_rows(&self) -> Option<u64> {
        match self.total_rows.load(Ordering::Relaxed) {
            UNKNOWN => None,
            n => Some(n),
        }
    }

    /// Set (or clear) the expected total number of rows.
    pub fn set_total_rows(&self, total: Option<u64>) {
        let value = match total {
            Some(n) => n.min(UNKNOWN - 1),
            None => UNKNOWN,
        };
        self.total_rows.store(value, Ordering::Relaxed);
    }

    /// Add `n` rows to the expected total (multi-item jobs learn totals per item).
    pub fn add_total_rows(&self, n: u64) {
        let mut cur = self.total_rows.load(Ordering::Relaxed);
        loop {
            let new = if cur == UNKNOWN { n.min(UNKNOWN - 1) } else { cur.saturating_add(n).min(UNKNOWN - 1) };
            match self.total_rows.compare_exchange_weak(cur, new, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Finished items of a multi-object job.
    pub fn items_done(&self) -> u64 {
        self.items_done.load(Ordering::Relaxed)
    }

    /// Total items of a multi-object job.
    pub fn items_total(&self) -> u64 {
        self.items_total.load(Ordering::Relaxed)
    }

    /// Set the number of items of a multi-object job.
    pub fn set_items_total(&self, n: u64) {
        self.items_total.store(n, Ordering::Relaxed);
    }

    /// Mark `name` as being processed.
    pub fn item_started(&self, name: &str) {
        self.state.lock().active_items.push(name.to_string());
    }

    /// Mark `name` as finished (successfully or not) and count it as done.
    pub fn item_finished(&self, name: &str) {
        let mut st = self.state.lock();
        if let Some(pos) = st.active_items.iter().position(|n| n == name) {
            st.active_items.remove(pos);
        }
        drop(st);
        self.items_done.fetch_add(1, Ordering::Relaxed);
    }

    /// Name of the item being processed (the first one when several run in parallel).
    pub fn current_item(&self) -> Option<String> {
        self.state.lock().active_items.first().cloned()
    }

    /// Every item currently being processed.
    pub fn active_items(&self) -> Vec<String> {
        self.state.lock().active_items.clone()
    }

    /// Failed rows or items.
    pub fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    /// Count `n` more failures.
    pub fn add_errors(&self, n: u64) {
        self.errors.fetch_add(n, Ordering::Relaxed);
    }

    /// Log a message.
    pub fn message(&self, level: MessageLevel, item: Option<&str>, text: impl Into<String>) {
        let msg = ProgressMessage {
            level,
            item: item.map(str::to_string),
            text: text.into(),
            elapsed_secs: self.elapsed().as_secs_f64(),
        };
        match level {
            MessageLevel::Error => tracing::debug!(item = ?msg.item, "{}", msg.text),
            _ => tracing::trace!(item = ?msg.item, "{}", msg.text),
        }
        let mut st = self.state.lock();
        if st.messages.len() >= MAX_MESSAGES {
            st.messages.pop_front();
            st.dropped_messages += 1;
        }
        st.messages.push_back(msg);
    }

    /// Log an informational message.
    pub fn info(&self, item: Option<&str>, text: impl Into<String>) {
        self.message(MessageLevel::Info, item, text);
    }

    /// Log a warning.
    pub fn warn(&self, item: Option<&str>, text: impl Into<String>) {
        self.message(MessageLevel::Warning, item, text);
    }

    /// Log an error.
    pub fn error(&self, item: Option<&str>, text: impl Into<String>) {
        self.message(MessageLevel::Error, item, text);
    }

    /// Copy of the retained messages (the most recent 1000).
    pub fn messages(&self) -> Vec<ProgressMessage> {
        self.state.lock().messages.iter().cloned().collect()
    }

    /// Remove and return the retained messages (for incremental UI polling).
    pub fn take_messages(&self) -> Vec<ProgressMessage> {
        self.state.lock().messages.drain(..).collect()
    }

    /// Messages discarded because more than 1000 were logged.
    pub fn dropped_messages(&self) -> u64 {
        self.state.lock().dropped_messages
    }

    /// Time since the progress was created.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Average rows per second since the start.
    pub fn rows_per_second(&self) -> f64 {
        let secs = self.elapsed().as_secs_f64();
        if secs <= 0.0 { 0.0 } else { self.rows() as f64 / secs }
    }

    /// Completed fraction in `0.0..=1.0`: rows of the known total, otherwise finished items.
    pub fn fraction(&self) -> Option<f64> {
        if let Some(total) = self.total_rows()
            && total > 0
        {
            return Some((self.rows() as f64 / total as f64).clamp(0.0, 1.0));
        }
        let items = self.items_total();
        if items > 0 {
            return Some((self.items_done() as f64 / items as f64).clamp(0.0, 1.0));
        }
        None
    }

    /// Consistent copy of all counters.
    pub fn snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            rows: self.rows(),
            total_rows: self.total_rows(),
            bytes: self.bytes(),
            items_done: self.items_done(),
            items_total: self.items_total(),
            active_items: self.active_items(),
            errors: self.errors(),
            elapsed_secs: self.elapsed().as_secs_f64(),
            rows_per_second: self.rows_per_second(),
            fraction: self.fraction(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_and_items() {
        let p = Progress::new();
        assert_eq!(p.total_rows(), None);
        p.add_rows(10);
        p.set_total_rows(Some(40));
        assert_eq!(p.total_rows(), Some(40));
        assert_eq!(p.fraction(), Some(0.25));
        p.set_total_rows(None);
        assert_eq!(p.total_rows(), None);
        p.add_total_rows(5);
        p.add_total_rows(5);
        assert_eq!(p.total_rows(), Some(10));
        p.set_items_total(2);
        p.item_started("a");
        p.item_started("b");
        assert_eq!(p.current_item().as_deref(), Some("a"));
        p.item_finished("a");
        assert_eq!(p.active_items(), vec!["b".to_string()]);
        assert_eq!(p.items_done(), 1);
        let s = p.snapshot();
        assert_eq!(s.rows, 10);
        assert_eq!(s.items_done, 1);
    }

    #[test]
    fn messages_are_bounded() {
        let p = Progress::new();
        for i in 0..(MAX_MESSAGES + 5) {
            p.warn(Some("t"), format!("m{i}"));
        }
        assert_eq!(p.messages().len(), MAX_MESSAGES);
        assert_eq!(p.dropped_messages(), 5);
        assert_eq!(p.messages()[0].text, "m5");
        assert_eq!(p.take_messages().len(), MAX_MESSAGES);
        assert!(p.messages().is_empty());
    }
}
