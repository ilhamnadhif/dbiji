//! Streaming 1,000,000 rows must not buffer the result: a counting global
//! allocator tracks the peak heap size while the rows are fetched in batches.

mod common;

use common::*;
use dbiji_core::{ExecOptions, ResultHeader, Session};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn note_alloc(size: usize) {
    let now = CURRENT.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

// SAFETY: forwards to the system allocator and only updates counters.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: same contract as the caller's.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            note_alloc(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: same contract as the caller's.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            note_alloc(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: same contract as the caller's.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
            note_alloc(new_size);
        }
        p
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const ROWS: usize = 1_000_000;
const PAD: usize = 100;

#[tokio::test]
async fn million_rows_stream_with_bounded_memory() {
    for srv in servers() {
        let mut s = srv.connect().await;
        let sql = numbers_sql(6, PAD);
        let baseline = CURRENT.load(Ordering::Relaxed);
        PEAK.store(baseline, Ordering::Relaxed);
        let started = Instant::now();
        let mut c = s.execute(&sql, &[], &ExecOptions::default()).await.unwrap();
        let Some(ResultHeader::Rows(_)) = c.next_result().await.unwrap() else {
            panic!("rows expected")
        };
        let mut total = 0usize;
        let mut checksum: u64 = 0;
        let mut payload_bytes = 0usize;
        loop {
            let batch = c.fetch(1000).await.unwrap();
            if batch.is_empty() {
                break;
            }
            for r in &batch {
                checksum += r[0].as_i64().unwrap_or(0) as u64;
                payload_bytes += r[1].as_str().map(str::len).unwrap_or(0) + 8;
            }
            total += batch.len();
        }
        assert!(c.next_result().await.unwrap().is_none());
        c.close().await.unwrap();
        drop(c);
        let elapsed = started.elapsed();
        let peak_growth = PEAK.load(Ordering::Relaxed).saturating_sub(baseline);
        println!(
            "[{}] {total} rows ({} MiB payload) in {elapsed:?}: {:.0} rows/s, peak heap growth {:.1} MiB",
            srv.name,
            payload_bytes / (1024 * 1024),
            total as f64 / elapsed.as_secs_f64(),
            peak_growth as f64 / (1024.0 * 1024.0)
        );
        assert_eq!(total, ROWS, "[{}]", srv.name);
        assert_eq!(checksum, (ROWS as u64 - 1) * ROWS as u64 / 2);
        // The payload alone is ~104 MiB (and several times that as Values);
        // streaming keeps the heap growth to a few batches.
        assert!(
            peak_growth < 32 * 1024 * 1024,
            "[{}] peak heap growth {peak_growth} bytes",
            srv.name
        );
        s.close().await.unwrap();
    }
}
