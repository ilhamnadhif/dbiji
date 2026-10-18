//! Memory bound and throughput of streaming exports.
//!
//! A tracking global allocator records the peak heap use, so the test can
//! assert that exporting many rows needs memory for about one batch only.
//! Run the benchmark alone with
//! `cargo test --release -p dbiji-transfer --test bench -- --ignored --nocapture`.

use async_trait::async_trait;
use chrono::NaiveDate;
use dbiji_core::{ColumnMeta, DataKind, DbResult, Row, Value};
use dbiji_transfer::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

struct Tracking;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call is forwarded to the system allocator unchanged; the
// wrapper only updates two counters.
unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                let grow = new_size - layout.size();
                let now = CURRENT.fetch_add(grow, Ordering::Relaxed) + grow;
                PEAK.fetch_max(now, Ordering::Relaxed);
            } else {
                CURRENT.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        p
    }
}

#[global_allocator]
static GLOBAL: Tracking = Tracking;

/// Generates rows on the fly, like a database cursor.
struct Generated {
    columns: Vec<ColumnMeta>,
    next: u64,
    total: u64,
}

impl Generated {
    fn new(total: u64) -> Self {
        Generated {
            columns: vec![
                ColumnMeta::new("id", "int8", DataKind::Int),
                ColumnMeta::new("name", "text", DataKind::Text),
                ColumnMeta::new("score", "float8", DataKind::Float),
                ColumnMeta::new("amount", "numeric(12,2)", DataKind::Decimal),
                ColumnMeta::new("created", "timestamp", DataKind::DateTime),
                ColumnMeta::new("active", "bool", DataKind::Bool),
                ColumnMeta::new("note", "text", DataKind::Text),
            ],
            next: 0,
            total,
        }
    }
}

#[async_trait]
impl RowSource for Generated {
    fn columns(&self) -> &[ColumnMeta] {
        &self.columns
    }

    async fn next_batch(&mut self, max: usize) -> DbResult<Vec<Row>> {
        let base = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap_or_default().and_hms_opt(0, 0, 0).unwrap_or_default();
        let n = (self.total - self.next).min(max as u64);
        let rows = (self.next..self.next + n)
            .map(|i| {
                vec![
                    Value::Int(i as i64),
                    Value::text(format!("Customer number {i}")),
                    Value::Float(i as f64 * 0.25),
                    Value::decimal(format!("{}.{:02}", i * 3, i % 100)),
                    Value::DateTime(base + chrono::Duration::seconds(i as i64)),
                    Value::Bool(i % 3 == 0),
                    if i % 10 == 0 { Value::Null } else { Value::text("needs \"quoting\", sometimes") },
                ]
            })
            .collect();
        self.next += n;
        Ok(rows)
    }

    fn total_rows(&self) -> Option<u64> {
        Some(self.total)
    }
}

/// Export `rows` generated rows; returns (summary, peak heap growth in bytes, seconds).
async fn export(format: ExportFormat, rows: u64, to_file: bool) -> (ExportSummary, usize, f64) {
    let dir = tempfile::tempdir().unwrap();
    let target = if to_file { ExportTarget::path(dir.path().join(format!("bench.{}", format.extension()))) } else { ExportTarget::writer(std::io::sink()) };
    let mut src = Generated::new(rows);
    let mut sink = create_sink(format, &ExportOptions::default(), target, None).unwrap();
    let progress = Progress::new();
    let cancel = CancellationToken::new();
    let baseline = CURRENT.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    let t0 = Instant::now();
    let summary = run_export_batched(&mut src, sink.as_mut(), 1000, &progress, &cancel).await.unwrap();
    let secs = t0.elapsed().as_secs_f64();
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(baseline);
    (summary, peak, secs)
}

/// One test function so no other test allocates while the peak is measured.
#[tokio::test]
async fn streaming_exports_have_bounded_memory() {
    let (small, small_peak, _) = export(ExportFormat::Csv, 20_000, true).await;
    let (big, big_peak, _) = export(ExportFormat::Csv, 200_000, true).await;
    assert_eq!(small.rows, 20_000);
    assert_eq!(big.rows, 200_000);
    assert!(big.bytes > 10 * 1024 * 1024, "{} bytes", big.bytes);
    // Ten times the rows, (almost) the same memory: one batch plus buffers.
    assert!(big_peak < 8 * 1024 * 1024, "peak heap growth {big_peak} bytes");
    assert!(big_peak < small_peak * 2 + 1024 * 1024, "small {small_peak}, big {big_peak}");

    for format in [ExportFormat::JsonLines, ExportFormat::Sql, ExportFormat::Xml, ExportFormat::Txt] {
        let (s, peak, _) = export(format, 100_000, false).await;
        assert_eq!(s.rows, 100_000);
        assert!(peak < 8 * 1024 * 1024, "{format:?}: peak heap growth {peak} bytes");
    }
    // Parquet buffers one row group (64k rows by default), XLSX one row.
    let (s, peak, _) = export(ExportFormat::Parquet, 200_000, true).await;
    assert_eq!(s.rows, 200_000);
    assert!(peak < 64 * 1024 * 1024, "parquet peak heap growth {peak} bytes");
    let (s, peak, _) = export(ExportFormat::Xlsx, 30_000, true).await;
    assert_eq!(s.rows, 30_000);
    assert!(peak < 32 * 1024 * 1024, "xlsx peak heap growth {peak} bytes");
}

#[tokio::test]
#[ignore = "benchmark: run with --release --ignored --nocapture"]
async fn benchmark_one_million_rows() {
    for (format, to_file) in [(ExportFormat::Csv, false), (ExportFormat::Csv, true), (ExportFormat::JsonLines, true), (ExportFormat::Sql, true), (ExportFormat::Parquet, true)] {
        let (s, peak, secs) = export(format, 1_000_000, to_file).await;
        assert_eq!(s.rows, 1_000_000);
        println!(
            "{:<10} {:<6} {:>9.0} rows/s  {:>7.1} MB written  peak heap +{:.1} MB  ({secs:.2} s)",
            format!("{format:?}"),
            if to_file { "file" } else { "sink" },
            s.rows as f64 / secs,
            s.bytes as f64 / 1e6,
            peak as f64 / 1e6,
        );
        if format == ExportFormat::Csv {
            assert!(peak < 8 * 1024 * 1024, "CSV peak heap growth {peak} bytes");
        }
    }
}
