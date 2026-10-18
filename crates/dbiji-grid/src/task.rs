//! Minimal background task runner for CPU work off the UI thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// State of a [`BackgroundTask`].
#[derive(Debug)]
pub enum TaskPoll<T> {
    /// Still running.
    Running,
    /// Finished.
    Done(T),
    /// Cancelled or the worker panicked / could not start.
    Failed,
}

/// A computation running on its own thread. Dropping the task cancels it.
pub struct BackgroundTask<T> {
    rx: mpsc::Receiver<Option<T>>,
    cancel: Arc<AtomicBool>,
    started: Instant,
}

impl<T> std::fmt::Debug for BackgroundTask<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackgroundTask").field("started", &self.started).finish()
    }
}

impl<T: Send + 'static> BackgroundTask<T> {
    /// Run `work` on a new thread. `work` should check the flag periodically
    /// and return `None` when it is set. `on_done` runs on the worker after
    /// the result was sent (e.g. to request a repaint).
    pub fn spawn(
        name: &str,
        work: impl FnOnce(&AtomicBool) -> Option<T> + Send + 'static,
        on_done: Box<dyn Fn() + Send>,
    ) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let flag = cancel.clone();
        let spawned = std::thread::Builder::new().name(name.to_string()).spawn(move || {
            let r = work(&flag);
            let _ = tx.send(r);
            on_done();
        });
        if let Err(e) = spawned {
            tracing::error!("cannot start background task {name}: {e}");
        }
        BackgroundTask { rx, cancel, started: Instant::now() }
    }

    /// Non-blocking poll.
    pub fn poll(&self) -> TaskPoll<T> {
        match self.rx.try_recv() {
            Ok(Some(v)) => TaskPoll::Done(v),
            Ok(None) => TaskPoll::Failed,
            Err(mpsc::TryRecvError::Empty) => TaskPoll::Running,
            Err(mpsc::TryRecvError::Disconnected) => TaskPoll::Failed,
        }
    }

    /// Block until finished.
    pub fn wait(self) -> TaskPoll<T> {
        match self.rx.recv() {
            Ok(Some(v)) => TaskPoll::Done(v),
            _ => TaskPoll::Failed,
        }
    }

    /// Ask the worker to stop.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Time since start.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

impl<T> Drop for BackgroundTask<T> {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
