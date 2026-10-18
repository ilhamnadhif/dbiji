//! Streaming a million rows keeps memory bounded. This test lives in its own
//! binary so the counting allocator only sees this test's allocations.

mod common;

use common::memory;
use dbiji_core::{ExecOptions, ResultHeader, Session, Value};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(n: usize) {
    let now = CURRENT.fetch_add(n, Ordering::Relaxed) + n;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grew(layout.size());
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
                grew(new_size - layout.size());
            } else {
                CURRENT.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        p
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[tokio::test]
async fn streams_a_million_rows_with_bounded_memory() {
    let mut s = memory().await;
    // generated on the fly by SQLite, nothing is materialised
    let sql = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 1000000) \
               SELECT x, printf('%090d', x) AS pad, x * 0.5 AS half FROM c";
    let mut cursor = s.execute(sql, &[], &ExecOptions::default()).await.unwrap();
    assert!(matches!(
        cursor.next_result().await.unwrap(),
        Some(ResultHeader::Rows(_))
    ));
    let baseline = CURRENT.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    let mut count = 0u64;
    let mut sum = 0i64;
    loop {
        let batch = cursor.fetch(1000).await.unwrap();
        if batch.is_empty() {
            break;
        }
        assert!(batch.len() <= 1000);
        for row in &batch {
            count += 1;
            if let Value::Int(i) = row[0] {
                sum += i;
            }
        }
        assert_eq!(batch[0][1].as_str().map(str::len), Some(90));
    }
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(baseline);
    cursor.close().await.unwrap();
    eprintln!("peak heap growth while streaming 1,000,000 rows: {peak} bytes");
    assert_eq!(count, 1_000_000);
    assert_eq!(sum, 500_000_500_000);
    // Buffered completely, the rows would need well over 150 MB (three values
    // and a 90 byte string per row); streaming holds about one batch.
    assert!(
        peak < 8 * 1024 * 1024,
        "peak heap growth while streaming: {peak} bytes"
    );
}
