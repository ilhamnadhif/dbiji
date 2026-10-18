//! Row storage with bounded memory.
//!
//! Result sets are stored in pages of rows. When the resident size of a store
//! exceeds its budget, or all stores together exceed the global limit, the
//! least recently used pages are encoded and written to an anonymous temporary
//! file, and loaded back on access. This keeps memory proportional to what is
//! displayed and cached, even for result sets with millions of rows.

use crate::value::{Row, Value};
use parking_lot::Mutex;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A row store shared between the fetching task and the UI.
pub type SharedRowStore = Arc<Mutex<RowStore>>;

static GLOBAL_RESIDENT: AtomicUsize = AtomicUsize::new(0);
static GLOBAL_LIMIT: AtomicUsize = AtomicUsize::new(192 * 1024 * 1024);

/// Bytes currently held in memory by all row stores.
pub fn global_resident_bytes() -> usize {
    GLOBAL_RESIDENT.load(Ordering::Relaxed)
}

/// Limit for all row stores together; stores spill pages when it is exceeded.
pub fn set_global_limit(bytes: usize) {
    GLOBAL_LIMIT.store(bytes.max(1024 * 1024), Ordering::Relaxed);
}

pub fn global_limit() -> usize {
    GLOBAL_LIMIT.load(Ordering::Relaxed)
}

/// Default memory budget of one store.
pub const DEFAULT_STORE_BUDGET: usize = 48 * 1024 * 1024;
const DEFAULT_PAGE_ROWS: usize = 512;

fn row_bytes(row: &Row) -> usize {
    std::mem::size_of::<Row>()
        + row.capacity() * std::mem::size_of::<Value>()
        + row.iter().map(Value::heap_size).sum::<usize>()
}

struct Page {
    rows: Option<Vec<Row>>,
    /// In-memory size when loaded.
    bytes: usize,
    last_used: u64,
    /// Location of the current on-disk copy.
    disk: Option<(u64, u32)>,
    /// In-memory content differs from the disk copy.
    dirty: bool,
}

struct SpillFile {
    file: File,
    end: u64,
}

pub struct RowStore {
    page_rows: usize,
    pages: Vec<Page>,
    len: usize,
    resident: usize,
    budget: usize,
    tick: u64,
    spill: Option<SpillFile>,
    spill_failed: bool,
}

impl Default for RowStore {
    fn default() -> Self {
        Self::new(DEFAULT_STORE_BUDGET)
    }
}

impl RowStore {
    pub fn new(budget_bytes: usize) -> Self {
        RowStore {
            page_rows: DEFAULT_PAGE_ROWS,
            pages: Vec::new(),
            len: 0,
            resident: 0,
            budget: budget_bytes.max(64 * 1024),
            tick: 0,
            spill: None,
            spill_failed: false,
        }
    }

    pub fn shared(budget_bytes: usize) -> SharedRowStore {
        Arc::new(Mutex::new(Self::new(budget_bytes)))
    }

    /// Rows per page (only effective before the first row is added).
    pub fn with_page_rows(mut self, rows: usize) -> Self {
        if self.len == 0 {
            self.page_rows = rows.max(1);
        }
        self
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bytes of rows currently held in memory.
    pub fn resident_bytes(&self) -> usize {
        self.resident
    }

    pub fn budget(&self) -> usize {
        self.budget
    }

    pub fn set_budget(&mut self, bytes: usize) {
        self.budget = bytes.max(64 * 1024);
        self.enforce(None);
    }

    /// Number of pages that currently live only on disk.
    pub fn spilled_pages(&self) -> usize {
        self.pages.iter().filter(|p| p.rows.is_none()).count()
    }

    fn add_resident(&mut self, n: usize) {
        self.resident += n;
        GLOBAL_RESIDENT.fetch_add(n, Ordering::Relaxed);
    }

    fn sub_resident(&mut self, n: usize) {
        let n = n.min(self.resident);
        self.resident -= n;
        GLOBAL_RESIDENT.fetch_sub(n, Ordering::Relaxed);
    }

    pub fn push(&mut self, row: Row) {
        let need_new = self
            .pages
            .last()
            .is_none_or(|p| p.rows.as_ref().is_none_or(|r| r.len() >= self.page_rows));
        if need_new {
            self.tick += 1;
            self.pages.push(Page {
                rows: Some(Vec::with_capacity(self.page_rows)),
                bytes: 0,
                last_used: self.tick,
                disk: None,
                dirty: true,
            });
        }
        let b = row_bytes(&row);
        let last = self.pages.len() - 1;
        let page = &mut self.pages[last];
        page.rows.as_mut().expect("tail page is resident").push(row);
        page.bytes += b;
        page.dirty = true;
        self.len += 1;
        self.add_resident(b);
        if self.resident > self.budget || global_resident_bytes() > global_limit() {
            self.enforce(Some(last));
        }
    }

    pub fn extend(&mut self, rows: impl IntoIterator<Item = Row>) {
        for r in rows {
            self.push(r);
        }
    }

    fn locate(&self, idx: usize) -> (usize, usize) {
        (idx / self.page_rows, idx % self.page_rows)
    }

    /// Make sure page `p` is resident; returns false if it could not be loaded.
    fn ensure_loaded(&mut self, p: usize) -> bool {
        self.tick += 1;
        self.pages[p].last_used = self.tick;
        if self.pages[p].rows.is_some() {
            return true;
        }
        let Some((offset, len)) = self.pages[p].disk else { return false };
        let Some(spill) = self.spill.as_mut() else { return false };
        let mut buf = vec![0u8; len as usize];
        if let Err(e) = spill
            .file
            .seek(SeekFrom::Start(offset))
            .and_then(|_| spill.file.read_exact(&mut buf))
        {
            tracing::error!("row store: failed to read spilled page: {e}");
            return false;
        }
        let Some(rows) = decode_page(&buf) else {
            tracing::error!("row store: corrupt spilled page");
            return false;
        };
        let bytes: usize = rows.iter().map(row_bytes).sum();
        let page = &mut self.pages[p];
        page.rows = Some(rows);
        page.bytes = bytes;
        page.dirty = false;
        self.add_resident(bytes);
        self.enforce(Some(p));
        true
    }

    /// Spill least recently used pages until within budget. `keep` is never evicted.
    fn enforce(&mut self, keep: Option<usize>) {
        if self.spill_failed {
            return;
        }
        let tail = self.pages.len().saturating_sub(1);
        while self.resident > self.budget || global_resident_bytes() > global_limit() {
            let victim = self
                .pages
                .iter()
                .enumerate()
                .filter(|(i, p)| p.rows.is_some() && Some(*i) != keep && *i != tail)
                .min_by_key(|(_, p)| p.last_used)
                .map(|(i, _)| i);
            let Some(v) = victim else { break };
            if !self.evict(v) {
                break;
            }
            if global_resident_bytes() <= global_limit() && self.resident <= self.budget {
                break;
            }
            // Over the global limit but within our own budget: stop after
            // shedding a bounded share so one store does not empty itself
            // because of others.
            if self.resident <= self.budget / 4 {
                break;
            }
        }
    }

    fn evict(&mut self, p: usize) -> bool {
        if self.pages[p].dirty || self.pages[p].disk.is_none() {
            if self.spill.is_none() {
                match tempfile::tempfile() {
                    Ok(file) => self.spill = Some(SpillFile { file, end: 0 }),
                    Err(e) => {
                        tracing::warn!("row store: cannot create spill file, keeping rows in memory: {e}");
                        self.spill_failed = true;
                        return false;
                    }
                }
            }
            let buf = encode_page(self.pages[p].rows.as_ref().expect("resident"));
            let spill = self.spill.as_mut().expect("spill file");
            let offset = spill.end;
            if let Err(e) = spill.file.seek(SeekFrom::Start(offset)).and_then(|_| spill.file.write_all(&buf)) {
                tracing::warn!("row store: spill write failed, keeping rows in memory: {e}");
                self.spill_failed = true;
                return false;
            }
            spill.end += buf.len() as u64;
            self.pages[p].disk = Some((offset, buf.len() as u32));
            self.pages[p].dirty = false;
        }
        let bytes = self.pages[p].bytes;
        self.pages[p].rows = None;
        self.pages[p].bytes = 0;
        self.sub_resident(bytes);
        true
    }

    /// Row at `idx`, loading it from disk if needed.
    pub fn get(&mut self, idx: usize) -> Option<&Row> {
        if idx >= self.len {
            return None;
        }
        let (p, o) = self.locate(idx);
        if !self.ensure_loaded(p) {
            return None;
        }
        self.pages[p].rows.as_ref()?.get(o)
    }

    pub fn get_cloned(&mut self, idx: usize) -> Option<Row> {
        self.get(idx).cloned()
    }

    pub fn cell(&mut self, idx: usize, col: usize) -> Option<&Value> {
        self.get(idx)?.get(col)
    }

    /// Replace row `idx`.
    pub fn set(&mut self, idx: usize, row: Row) -> bool {
        if idx >= self.len {
            return false;
        }
        let (p, o) = self.locate(idx);
        if !self.ensure_loaded(p) {
            return false;
        }
        let nb = row_bytes(&row);
        let page = &mut self.pages[p];
        let rows = page.rows.as_mut().expect("loaded");
        let ob = row_bytes(&rows[o]);
        rows[o] = row;
        page.bytes = (page.bytes + nb).saturating_sub(ob);
        page.dirty = true;
        if nb >= ob {
            self.add_resident(nb - ob);
        } else {
            self.sub_resident(ob - nb);
        }
        true
    }

    /// Replace one cell.
    pub fn set_cell(&mut self, idx: usize, col: usize, value: Value) -> bool {
        match self.get_cloned(idx) {
            Some(mut row) if col < row.len() => {
                row[col] = value;
                self.set(idx, row)
            }
            _ => false,
        }
    }

    /// Visit rows in `start..end` sequentially (export, client side sort/filter).
    pub fn for_each_in(&mut self, start: usize, end: usize, mut f: impl FnMut(usize, &Row)) {
        let end = end.min(self.len);
        let mut i = start;
        while i < end {
            let (p, o) = self.locate(i);
            if !self.ensure_loaded(p) {
                i = (p + 1) * self.page_rows;
                continue;
            }
            let rows = self.pages[p].rows.as_ref().expect("loaded");
            let upto = rows.len().min(o + (end - i));
            for (k, row) in rows[o..upto].iter().enumerate() {
                f(i + k, row);
            }
            i += upto - o;
            if upto == o {
                break;
            }
        }
    }

    pub fn for_each(&mut self, f: impl FnMut(usize, &Row)) {
        let len = self.len;
        self.for_each_in(0, len, f);
    }

    /// Copy rows `start..end` out of the store.
    pub fn slice(&mut self, start: usize, end: usize) -> Vec<Row> {
        let mut out = Vec::with_capacity(end.saturating_sub(start).min(1 << 20));
        self.for_each_in(start, end, |_, r| out.push(r.clone()));
        out
    }

    /// Values of one column (for sorting / grouping / charts).
    pub fn column_values(&mut self, col: usize) -> Vec<Value> {
        let mut out = Vec::with_capacity(self.len);
        self.for_each(|_, r| out.push(r.get(col).cloned().unwrap_or(Value::Null)));
        out
    }

    /// Drop all rows and the spill file.
    pub fn clear(&mut self) {
        let r = self.resident;
        self.sub_resident(r);
        self.pages.clear();
        self.len = 0;
        self.spill = None;
        self.spill_failed = false;
    }

    /// Spill everything except the tail page (used for inactive result tabs).
    pub fn release_memory(&mut self) {
        let saved = self.budget;
        self.budget = 64 * 1024;
        self.enforce(None);
        self.budget = saved;
    }
}

impl Drop for RowStore {
    fn drop(&mut self) {
        let r = self.resident;
        GLOBAL_RESIDENT.fetch_sub(r.min(GLOBAL_RESIDENT.load(Ordering::Relaxed)), Ordering::Relaxed);
    }
}

fn encode_page(rows: &[Row]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(rows.len() * 64);
    buf.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    for r in rows {
        buf.extend_from_slice(&(r.len() as u32).to_le_bytes());
        for v in r {
            v.encode(&mut buf);
        }
    }
    buf
}

fn decode_page(mut input: &[u8]) -> Option<Vec<Row>> {
    fn u32_(input: &mut &[u8]) -> Option<u32> {
        if input.len() < 4 {
            return None;
        }
        let (a, b) = input.split_at(4);
        *input = b;
        Some(u32::from_le_bytes(a.try_into().ok()?))
    }
    let n = u32_(&mut input)? as usize;
    let mut rows = Vec::with_capacity(n);
    for _ in 0..n {
        let c = u32_(&mut input)? as usize;
        let mut row = Vec::with_capacity(c);
        for _ in 0..c {
            row.push(Value::decode(&mut input)?);
        }
        rows.push(row);
    }
    Some(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(i: usize) -> Row {
        vec![Value::Int(i as i64), Value::text(format!("row number {i} with some padding text"))]
    }

    #[test]
    fn spills_and_reloads() {
        let mut s = RowStore::new(256 * 1024).with_page_rows(100);
        for i in 0..50_000 {
            s.push(row(i));
        }
        assert_eq!(s.len(), 50_000);
        assert!(s.resident_bytes() <= 256 * 1024 + 100 * 200, "resident {}", s.resident_bytes());
        assert!(s.spilled_pages() > 0);
        for i in [0usize, 1, 99, 100, 12_345, 49_999] {
            assert_eq!(s.get(i).unwrap(), &row(i));
        }
        assert!(s.get(50_000).is_none());
        // modify a spilled row and read it back after it has been evicted again
        assert!(s.set_cell(10, 1, Value::text("changed")));
        for i in 20_000..30_000 {
            let _ = s.get(i);
        }
        assert_eq!(s.cell(10, 1).unwrap(), &Value::text("changed"));
        let mut count = 0;
        s.for_each(|i, r| {
            assert_eq!(r[0], Value::Int(i as i64));
            count += 1;
        });
        assert_eq!(count, 50_000);
        assert_eq!(s.slice(5, 8).len(), 3);
    }

    #[test]
    fn release_memory_keeps_data() {
        let mut s = RowStore::new(64 * 1024 * 1024).with_page_rows(64);
        for i in 0..5_000 {
            s.push(row(i));
        }
        let before = s.resident_bytes();
        s.release_memory();
        assert!(s.resident_bytes() < before);
        assert_eq!(s.get(4_321).unwrap(), &row(4_321));
    }
}
