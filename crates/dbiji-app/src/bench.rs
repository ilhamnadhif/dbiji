//! Startup measurement and headless benchmarks (`dbiji --bench ...`).

use std::sync::OnceLock;
use std::time::Instant;

static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// Record the process start as early as possible (called first in `main`).
pub fn mark_process_start() {
    let _ = PROCESS_START.set(Instant::now());
}

/// Called on the first rendered frame.
pub fn report_first_frame(app_created: Instant) {
    let start = PROCESS_START.get().copied().unwrap_or(app_created);
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    tracing::info!("first frame after {ms:.1} ms");
    if std::env::var_os("DBIJI_PRINT_STARTUP").is_some() {
        println!("startup_ms={ms:.1}");
    }
}
