//! Bridge between the egui UI thread and the tokio runtime.
//!
//! The UI never blocks: every database or file operation is spawned on the
//! runtime and returns a [`Job`] that the UI polls once per frame. Finished
//! jobs request a repaint, so idle CPU stays at zero while waiting.

use parking_lot::Mutex;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Owns the tokio runtime used for all I/O.
pub struct Backend {
    runtime: tokio::runtime::Runtime,
    ctx: Mutex<Option<egui::Context>>,
}

impl Backend {
    pub fn new() -> std::io::Result<Self> {
        let workers = std::thread::available_parallelism().map(|n| n.get().clamp(2, 4)).unwrap_or(2);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .thread_name("dbiji-io")
            .enable_all()
            .build()?;
        Ok(Backend { runtime, ctx: Mutex::new(None) })
    }

    /// Attach the egui context so finished jobs can trigger repaints.
    pub fn set_context(&self, ctx: egui::Context) {
        *self.ctx.lock() = Some(ctx);
    }

    pub fn handle(&self) -> tokio::runtime::Handle {
        self.runtime.handle().clone()
    }

    pub fn repainter(&self) -> Repainter {
        Repainter { ctx: self.ctx.lock().clone() }
    }

    /// Run a future on the runtime; the result is picked up with [`Job::poll`].
    pub fn spawn<F, T>(&self, fut: F) -> Job<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let slot: Arc<Mutex<Option<T>>> = Arc::new(Mutex::new(None));
        let done = Arc::new(AtomicBool::new(false));
        let repaint = self.repainter();
        let (s, d) = (slot.clone(), done.clone());
        let handle = self.runtime.spawn(async move {
            let out = fut.await;
            *s.lock() = Some(out);
            d.store(true, Ordering::Release);
            repaint.request();
        });
        Job { slot, done, handle: Some(handle), taken: false }
    }

    /// Fire-and-forget task.
    pub fn spawn_detached<F>(&self, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.runtime.spawn(fut);
    }

    /// Block the current (non-UI) thread on a future. Used by CLI modes only.
    pub fn block_on<F: Future>(&self, fut: F) -> F::Output {
        self.runtime.block_on(fut)
    }
}

/// Cheap handle used by background tasks to wake the UI.
#[derive(Clone, Default)]
pub struct Repainter {
    ctx: Option<egui::Context>,
}

impl Repainter {
    pub fn request(&self) {
        if let Some(ctx) = &self.ctx {
            ctx.request_repaint();
        }
    }
}

/// Result of a background operation, polled by the UI.
pub struct Job<T> {
    slot: Arc<Mutex<Option<T>>>,
    done: Arc<AtomicBool>,
    handle: Option<tokio::task::JoinHandle<()>>,
    taken: bool,
}

impl<T> Job<T> {
    /// Take the result once it is available.
    pub fn poll(&mut self) -> Option<T> {
        if self.taken || !self.done.load(Ordering::Acquire) {
            return None;
        }
        let v = self.slot.lock().take();
        if v.is_some() {
            self.taken = true;
        }
        v
    }

    pub fn is_finished(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// Abort the underlying task (the future is dropped at its next await point).
    pub fn abort(&mut self) {
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

impl<T> Drop for Job<T> {
    fn drop(&mut self) {
        // Results nobody waits for are simply dropped; the task keeps running
        // to completion so sessions are never left in a half-used state.
    }
}

/// Value that is loaded in the background.
pub enum Loadable<T> {
    NotLoaded,
    Loading(Job<dbiji_core::DbResult<T>>),
    Loaded(T),
    Failed(dbiji_core::DbError),
}

impl<T> Default for Loadable<T> {
    fn default() -> Self {
        Loadable::NotLoaded
    }
}

impl<T> Loadable<T> {
    /// Move a finished job into `Loaded`/`Failed`. Returns true when the state changed.
    pub fn poll(&mut self) -> bool {
        if let Loadable::Loading(job) = self {
            if let Some(res) = job.poll() {
                *self = match res {
                    Ok(v) => Loadable::Loaded(v),
                    Err(e) => Loadable::Failed(e),
                };
                return true;
            }
        }
        false
    }

    pub fn get(&self) -> Option<&T> {
        match self {
            Loadable::Loaded(v) => Some(v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self) -> Option<&mut T> {
        match self {
            Loadable::Loaded(v) => Some(v),
            _ => None,
        }
    }

    pub fn is_loading(&self) -> bool {
        matches!(self, Loadable::Loading(_))
    }

    pub fn is_not_loaded(&self) -> bool {
        matches!(self, Loadable::NotLoaded)
    }

    pub fn error(&self) -> Option<&dbiji_core::DbError> {
        match self {
            Loadable::Failed(e) => Some(e),
            _ => None,
        }
    }
}
