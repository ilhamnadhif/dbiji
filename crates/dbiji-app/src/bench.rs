//! Startup measurement and automated benchmarks (`dbiji --bench <name>`),
//! used by CI to verify the PRD §2 targets (startup < 1 s, idle memory
//! < 120 MB, 3 connections + 5 result tabs < 250 MB, < 16 ms per frame).

use serde::Serialize;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

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

#[derive(Debug, Clone, Serialize)]
pub struct BenchResult {
    pub name: String,
    pub value: f64,
    pub unit: String,
    pub target: Option<f64>,
    pub passed: bool,
    pub detail: String,
}

impl BenchResult {
    fn new(name: &str, value: f64, unit: &str, target: Option<f64>, detail: String) -> Self {
        BenchResult {
            name: name.into(),
            value,
            unit: unit.into(),
            target,
            passed: target.is_none_or(|t| value <= t),
            detail,
        }
    }
}

/// Resident set size of a process in bytes (macOS / Linux via `ps`).
pub fn rss_of(pid: u32) -> Option<u64> {
    let out = Command::new("ps").args(["-o", "rss=", "-p", &pid.to_string()]).output().ok()?;
    let kb: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    Some(kb * 1024)
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if v.is_empty() { 0.0 } else { v[v.len() / 2] }
}

/// Benchmarks run the app against a throw-away configuration directory so
/// they never read or modify the user's settings, connections or history.
fn bench_home() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("dbiji-bench-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn bench_startup() -> BenchResult {
    let exe = std::env::current_exe().unwrap_or_default();
    let home = bench_home();
    let mut samples = Vec::new();
    for _ in 0..5 {
        let t0 = Instant::now();
        let out = Command::new(&exe)
            .env("DBIJI_HOME", &home)
            .arg("--exit-after-first-frame")
            .env("DBIJI_PRINT_STARTUP", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        let wall = t0.elapsed().as_secs_f64() * 1000.0;
        let reported = out
            .ok()
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .find_map(|l| l.strip_prefix("startup_ms=").and_then(|v| v.trim().parse::<f64>().ok()))
            })
            .unwrap_or(wall);
        samples.push(reported);
    }
    let m = median(samples.clone());
    BenchResult::new("startup_first_frame", m, "ms", Some(1000.0), format!("samples: {samples:?}"))
}

/// Start the app with a scenario and sample its memory after it settles.
fn bench_memory(scenario: &str, settle: Duration, target_mb: f64) -> BenchResult {
    let exe = std::env::current_exe().unwrap_or_default();
    let child = Command::new(&exe)
        .env("DBIJI_HOME", bench_home())
        .env("DBIJI_BENCH_SCENARIO", scenario)
        .env("DBIJI_BENCH_EXIT_AFTER_SECS", (settle.as_secs() + 3).to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return BenchResult::new(&format!("memory_{scenario}"), f64::NAN, "MB", Some(target_mb), "cannot start".into());
    };
    std::thread::sleep(settle);
    let rss = rss_of(child.id()).unwrap_or(0) as f64 / 1_048_576.0;
    let _ = child.kill();
    let _ = child.wait();
    BenchResult::new(&format!("memory_{scenario}"), rss, "MB", Some(target_mb), String::new())
}

/// Run benchmarks: `startup`, `memory`, `all`. Returns the process exit code.
pub fn run(name: &str, report: Option<&str>) -> std::process::ExitCode {
    let mut results = Vec::new();
    let all = name == "all";
    if all || name == "startup" {
        results.push(bench_startup());
    }
    if all || name == "memory" {
        results.push(bench_memory("idle", Duration::from_secs(4), 120.0));
        results.push(bench_memory("workload", Duration::from_secs(12), 250.0));
    }
    if all || name == "grid" || name == "editor" {
        results.extend(crate::bench_ui::run(name));
    }
    for r in &results {
        println!(
            "{:<28} {:>10.1} {:<3} target {:<8} {}",
            r.name,
            r.value,
            r.unit,
            r.target.map(|t| format!("{t}")).unwrap_or_else(|| "-".into()),
            if r.passed { "PASS" } else { "FAIL" }
        );
    }
    let _ = std::fs::remove_dir_all(bench_home());
    if let Some(path) = report
        && let Ok(json) = serde_json::to_string_pretty(&results) {
            let _ = std::fs::write(path, json);
        }
    if results.iter().all(|r| r.passed) { std::process::ExitCode::SUCCESS } else { std::process::ExitCode::FAILURE }
}
