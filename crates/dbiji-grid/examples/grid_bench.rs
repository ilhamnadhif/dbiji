//! Headless frame-time benchmark of the data grid.
//!
//! Generates 1,000,000 rows × 100 columns of mixed types into a spilling
//! `RowStore`, then runs egui frames through `Context::run_ui` (plus
//! tessellation) at a fixed window size while scrolling in several patterns,
//! and prints the CPU time per frame.
//!
//! ```text
//! cargo run --release -p dbiji-grid --example grid_bench [-- rows cols]
//! ```

#[path = "support/datagen.rs"]
mod datagen;

use dbiji_core::RowStore;
use dbiji_grid::{DataGrid, DataGridState, GridOptions, ResultModel};
use egui::{Event, MouseWheelUnit, Pos2, RawInput, Rect, TouchPhase, vec2};
use std::time::{Duration, Instant};

struct Stats {
    name: &'static str,
    times: Vec<f64>,
}

impl Stats {
    fn report(&self) {
        let mut t = self.times.clone();
        t.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let avg = t.iter().sum::<f64>() / t.len().max(1) as f64;
        let p95 = t.get((t.len() as f64 * 0.95) as usize).copied().unwrap_or(0.0);
        let max = t.last().copied().unwrap_or(0.0);
        println!("{:<38} frames {:>4}  avg {:>6.3} ms  p95 {:>6.3} ms  max {:>6.3} ms", self.name, t.len(), avg, p95, max);
    }
}

struct Bench {
    ctx: egui::Context,
    model: ResultModel,
    state: DataGridState,
    options: GridOptions,
    size: egui::Vec2,
    ppp: f32,
    time: f64,
}

impl Bench {
    /// Run one frame; returns CPU ms (egui pass + tessellation) and whether a repaint was requested.
    fn frame(&mut self, events: Vec<Event>) -> (f64, bool) {
        self.time += 1.0 / 60.0;
        let mut viewports = egui::ViewportIdMap::default();
        viewports.insert(egui::ViewportId::ROOT, egui::ViewportInfo { native_pixels_per_point: Some(self.ppp), focused: Some(true), ..Default::default() });
        let input = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, self.size)), time: Some(self.time), events, focused: true, viewports, ..Default::default() };
        let (model, state, options) = (&mut self.model, &mut self.state, &self.options);
        let start = Instant::now();
        let mut out = self.ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let _ = DataGrid::new(model, state, options).show(ui);
            });
        });
        let shapes = std::mem::take(&mut out.shapes);
        let prims = self.ctx.tessellate(shapes, out.pixels_per_point);
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        std::hint::black_box(prims.len());
        out.textures_delta.clear();
        let repaint = out.viewport_output.get(&egui::ViewportId::ROOT).is_some_and(|v| v.repaint_delay < Duration::from_secs(1));
        (elapsed, repaint)
    }

    fn wheel(&self, dy: f32, dx: f32) -> Vec<Event> {
        vec![
            Event::PointerMoved(Pos2::new(self.size.x / 2.0, self.size.y / 2.0)),
            Event::MouseWheel { unit: MouseWheelUnit::Point, delta: vec2(dx, dy), phase: TouchPhase::Move, modifiers: Default::default() },
        ]
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rows: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1_000_000);
    let cols: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(100);
    println!("Generating {rows} rows × {cols} columns …");
    let t = Instant::now();
    let store = RowStore::shared(dbiji_core::rowstore::DEFAULT_STORE_BUDGET);
    datagen::fill(&store, rows, cols, 42);
    {
        let s = store.lock();
        println!("  done in {:.1} s; resident {:.1} MB, spilled pages {}", t.elapsed().as_secs_f64(), s.resident_bytes() as f64 / 1_048_576.0, s.spilled_pages());
    }
    let mut model = ResultModel::new(datagen::columns(cols), store);
    let target = dbiji_grid::determine_editability(model.columns(), |_| Some(vec!["id".into()]), false);
    model.set_editability(target);
    model.fetch.has_more = false;
    let state = DataGridState::for_model(&model);

    for (label, size, ppp) in [("1600x1000 @2x", vec2(1600.0, 1000.0), 2.0f32), ("2560x1440 @1x", vec2(2560.0, 1440.0), 1.0)] {
        println!("\nWindow {label}");
        let mut b = Bench { ctx: egui::Context::default(), model, state: state.clone(), options: GridOptions::default(), size, ppp, time: 0.0 };
        // Warm up (font atlas, glyph cache).
        for _ in 0..5 {
            b.frame(vec![]);
        }
        let mut all = Vec::new();

        let mut s = Stats { name: "idle (no input)", times: Vec::new() };
        let mut idle_repaints = 0;
        for _ in 0..60 {
            let (ms, repaint) = b.frame(vec![]);
            s.times.push(ms);
            idle_repaints += repaint as usize;
        }
        s.report();
        println!("  repaint requested on {idle_repaints} of 60 idle frames");
        all.push(s);

        let mut s = Stats { name: "wheel scroll (3 rows / frame)", times: Vec::new() };
        for _ in 0..300 {
            let ev = b.wheel(-66.0, 0.0);
            s.times.push(b.frame(ev).0);
        }
        s.report();
        all.push(s);

        let mut s = Stats { name: "page down (new page every frame)", times: Vec::new() };
        for _ in 0..200 {
            let (x, y) = b.state.scroll_offset();
            b.state.set_scroll_offset(x, y + size.y as f64);
            s.times.push(b.frame(vec![]).0);
        }
        s.report();
        all.push(s);

        let mut s = Stats { name: "random jumps (scrollbar drag)", times: Vec::new() };
        let mut rng = datagen::Rng::new(7);
        for _ in 0..200 {
            let y = rng.below(rows as u64) as f64 * 22.0;
            b.state.set_scroll_offset(0.0, y);
            s.times.push(b.frame(vec![]).0);
        }
        s.report();
        all.push(s);

        let mut s = Stats { name: "horizontal scroll (40 px / frame)", times: Vec::new() };
        b.state.set_scroll_offset(0.0, 500_000.0 * 22.0);
        for _ in 0..300 {
            let ev = b.wheel(0.0, -40.0);
            s.times.push(b.frame(ev).0);
        }
        s.report();
        all.push(s);

        let mut s = Stats { name: "select all + keyboard navigation", times: Vec::new() };
        for i in 0..100 {
            let key = if i % 2 == 0 { egui::Key::PageDown } else { egui::Key::ArrowDown };
            let ev = vec![Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() }];
            s.times.push(b.frame(ev).0);
        }
        s.report();
        all.push(s);

        // A fetch task appends rows (2,048 per lock, spilling pages to disk under the lock) while the user scrolls.
        let mut s = Stats { name: "scroll while fetch task appends", times: Vec::new() };
        let store = b.model.store().clone();
        let start_len = store.lock().len();
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = done.clone();
        let fetcher = std::thread::spawn(move || {
            let mut rng = datagen::Rng::new(99);
            let mut hold_max = 0.0f64;
            let mut holds: Vec<f64> = Vec::new();
            let mut batch = Vec::with_capacity(2048);
            for i in start_len..start_len + 100_000 {
                batch.push(datagen::row(i, cols, &mut rng));
                if batch.len() == 2048 {
                    let mut g = store.lock();
                    let t = Instant::now();
                    g.extend(batch.drain(..));
                    let held = t.elapsed().as_secs_f64() * 1000.0;
                    drop(g);
                    hold_max = hold_max.max(held);
                    holds.push(held);
                }
                if flag.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
            }
            store.lock().extend(batch.drain(..));
            holds.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let p95 = holds.get(holds.len() * 95 / 100).copied().unwrap_or(0.0);
            println!("  fetcher: {} batches, lock held p95 {p95:.3} ms, max {hold_max:.3} ms", holds.len());
        });
        b.model.fetch.fetching = true;
        b.state.set_scroll_offset(0.0, 0.0);
        for _ in 0..300 {
            let ev = b.wheel(-44.0, 0.0);
            s.times.push(b.frame(ev).0);
        }
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = fetcher.join();
        b.model.fetch.fetching = false;
        b.frame(vec![]);
        println!("  (rows now {})", b.model.row_count());
        s.report();
        all.push(s);

        let worst = all.iter().flat_map(|s| s.times.iter().copied()).fold(0.0f64, f64::max);
        let total: Vec<f64> = all.iter().skip(1).flat_map(|s| s.times.iter().copied()).collect();
        let avg = total.iter().sum::<f64>() / total.len().max(1) as f64;
        println!("  => scrolling frames: avg {avg:.3} ms, worst {worst:.3} ms (budget 16 ms)");
        model = b.model;
    }
}
