//! Headless UI frame-time benchmarks (`dbiji --bench grid|editor`): the real
//! grid and editor widgets are driven through egui frames (layout +
//! tessellation, no GPU) and the CPU time per frame is compared with the PRD
//! budget of 16 ms.

use crate::bench::BenchResult;
use dbiji_core::{ColumnMeta, DataKind, RowStore, Value};
use egui::{Event, MouseWheelUnit, Pos2, RawInput, Rect, TouchPhase, vec2};
use std::time::Instant;

struct Frames {
    ctx: egui::Context,
    time: f64,
    size: egui::Vec2,
}

impl Frames {
    fn new() -> Self {
        let ctx = egui::Context::default();
        crate::theme::setup_fonts(&ctx);
        Frames { ctx, time: 0.0, size: vec2(1600.0, 1000.0) }
    }

    fn frame(&mut self, events: Vec<Event>, mut draw: impl FnMut(&mut egui::Ui)) -> f64 {
        self.time += 1.0 / 60.0;
        let input = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, self.size)), time: Some(self.time), events, focused: true, ..Default::default() };
        let t0 = Instant::now();
        let mut out = self.ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| draw(ui));
        });
        let shapes = std::mem::take(&mut out.shapes);
        let prims = self.ctx.tessellate(shapes, out.pixels_per_point);
        std::hint::black_box(prims.len());
        out.textures_delta.clear();
        t0.elapsed().as_secs_f64() * 1000.0
    }

    fn wheel(&self, dy: f32) -> Vec<Event> {
        vec![
            Event::PointerMoved(Pos2::new(self.size.x / 2.0, self.size.y / 2.0)),
            Event::MouseWheel { unit: MouseWheelUnit::Point, delta: vec2(0.0, dy), phase: TouchPhase::Move, modifiers: Default::default() },
        ]
    }
}

fn stats(name: &str, mut t: Vec<f64>) -> BenchResult {
    t.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let avg = t.iter().sum::<f64>() / t.len().max(1) as f64;
    let p95 = t.get((t.len() as f64 * 0.95) as usize).copied().unwrap_or(0.0);
    let max = t.last().copied().unwrap_or(0.0);
    BenchResult {
        name: name.into(),
        value: p95,
        unit: "ms".into(),
        target: Some(16.0),
        passed: p95 <= 16.0,
        detail: format!("p95 of {} frames; avg {avg:.2} ms, max {max:.2} ms", t.len()),
    }
}

/// 1 000 000 rows × 20 mixed columns in a spilling row store; wheel scrolling,
/// page jumps and horizontal scrolling.
fn grid() -> Vec<BenchResult> {
    let cols = 20usize;
    let rows = 1_000_000usize;
    let columns: Vec<ColumnMeta> = (0..cols)
        .map(|c| match c % 4 {
            0 => ColumnMeta::new(format!("id_{c}"), "int8", DataKind::Int),
            1 => ColumnMeta::new(format!("name_{c}"), "text", DataKind::Text),
            2 => ColumnMeta::new(format!("amount_{c}"), "numeric", DataKind::Decimal),
            _ => ColumnMeta::new(format!("flag_{c}"), "bool", DataKind::Bool),
        })
        .collect();
    let store = RowStore::shared(dbiji_core::rowstore::DEFAULT_STORE_BUDGET);
    {
        let mut batch = Vec::with_capacity(2000);
        for r in 0..rows {
            let row: Vec<Value> = (0..cols)
                .map(|c| match c % 4 {
                    0 => Value::Int((r * cols + c) as i64),
                    1 => Value::text(format!("row {r} column {c}")),
                    2 => Value::decimal(format!("{}.{:02}", r % 10_000, c)),
                    _ => Value::Bool((r + c) % 2 == 0),
                })
                .collect();
            batch.push(row);
            if batch.len() == 2000 {
                store.lock().extend(std::mem::take(&mut batch));
            }
        }
        store.lock().extend(batch);
    }
    let mut model = dbiji_grid::ResultModel::new(columns, store);
    model.fetch.has_more = false;
    let mut state = dbiji_grid::DataGridState::for_model(&model);
    let options = dbiji_grid::GridOptions::default();
    let mut f = Frames::new();
    for _ in 0..5 {
        f.frame(vec![], |ui| {
            dbiji_grid::DataGrid::new(&mut model, &mut state, &options).show(ui);
        });
    }
    let mut times = Vec::new();
    for _ in 0..240 {
        let ev = f.wheel(-60.0);
        times.push(f.frame(ev, |ui| {
            dbiji_grid::DataGrid::new(&mut model, &mut state, &options).show(ui);
        }));
    }
    let mut jumps = Vec::new();
    for i in 0..60 {
        let ev = vec![Event::Key {
            key: if i % 2 == 0 { egui::Key::PageDown } else { egui::Key::End },
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Default::default(),
        }];
        jumps.push(f.frame(ev, |ui| {
            dbiji_grid::DataGrid::new(&mut model, &mut state, &options).show(ui);
        }));
    }
    vec![stats("grid_1m_rows_scroll", times), stats("grid_1m_rows_jumps", jumps)]
}

/// 50 000-line script: typing and scrolling.
fn editor() -> Vec<BenchResult> {
    let mut text = String::with_capacity(3_000_000);
    for i in 0..50_000 {
        text.push_str(&format!("SELECT id, name, amount FROM sales.orders o JOIN customers c ON c.id = o.customer_id WHERE o.id = {i};\n"));
    }
    let lang = crate::sqllang::SqlLanguage::new(std::sync::Arc::new(dbiji_core::dialect::GenericDialect::new()));
    let options = dbiji_editor::EditorOptions { cursor_blink: false, ..Default::default() };
    let theme = dbiji_editor::EditorTheme::dark();
    let mut state = dbiji_editor::EditorState::new("bench_editor", text);
    let mut f = Frames::new();
    for _ in 0..5 {
        f.frame(vec![], |ui| {
            dbiji_editor::SqlEditor::new(&mut state, &lang, &options, &theme).show(ui);
        });
    }
    state.request_focus(&f.ctx);
    state.set_cursor(&f.ctx, 1_000);
    let mut typing = Vec::new();
    for i in 0..200 {
        let ch = if i % 10 == 9 { " " } else { "x" };
        typing.push(f.frame(vec![Event::Text(ch.into())], |ui| {
            dbiji_editor::SqlEditor::new(&mut state, &lang, &options, &theme).show(ui);
        }));
    }
    let mut scroll = Vec::new();
    for _ in 0..200 {
        let ev = f.wheel(-120.0);
        scroll.push(f.frame(ev, |ui| {
            dbiji_editor::SqlEditor::new(&mut state, &lang, &options, &theme).show(ui);
        }));
    }
    vec![stats("editor_50k_lines_typing", typing), stats("editor_50k_lines_scroll", scroll)]
}

pub fn run(name: &str) -> Vec<BenchResult> {
    let mut out = Vec::new();
    if name == "all" || name == "grid" {
        out.extend(grid());
    }
    if name == "all" || name == "editor" {
        out.extend(editor());
    }
    out
}
