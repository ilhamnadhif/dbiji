//! Headless benchmark of the ER diagram: layout time and frame time
//! (UI pass + tessellation) for a generated 200 table / ~2,000 column schema.
//!
//! `cargo run -p dbiji-visual --release --example er_bench [tables]`

#[path = "common/demo_data.rs"]
mod demo_data;

use dbiji_visual::er::{ErDiagram, ErModel, ErViewState, LayoutOptions, auto_layout, show_er};
use egui::{Event, Pos2, RawInput, Rect, vec2};
use std::time::{Duration, Instant};

fn stats(samples: &mut [Duration]) -> (f64, f64, f64, f64) {
    samples.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let n = samples.len().max(1);
    let avg = samples.iter().map(|d| ms(*d)).sum::<f64>() / n as f64;
    let p50 = ms(samples[n / 2]);
    let p95 = ms(samples[(n * 95 / 100).min(n - 1)]);
    let max = ms(samples[n - 1]);
    (avg, p50, p95, max)
}

fn main() {
    let tables: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(200);
    let snapshot = demo_data::schema(tables, 42);
    let model = ErModel::from_snapshot(&snapshot, None);
    let columns: usize = model.tables.iter().map(|t| t.columns.len()).sum();
    println!(
        "schema: {} tables/views, {} columns, {} relations",
        model.tables.len(),
        columns,
        model.relations.len()
    );

    // ---- layout
    let opts = LayoutOptions::default();
    let _warmup = auto_layout(&model, &opts);
    let mut samples = Vec::new();
    for _ in 0..30 {
        let t = Instant::now();
        let pos = auto_layout(&model, &opts);
        samples.push(t.elapsed());
        std::hint::black_box(pos);
    }
    let (avg, p50, p95, max) = stats(&mut samples);
    println!("auto_layout: avg {avg:.2} ms, p50 {p50:.2} ms, p95 {p95:.2} ms, max {max:.2} ms");

    // ---- frames
    let mut diagram = ErDiagram::new("bench", model);
    let ctx = egui::Context::default();
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);
    // 1600 x 1000 points on a retina display (2 physical pixels per point)
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(1600.0, 1000.0));
    let mut time = 0.0;
    let mut frame = |ctx: &egui::Context, d: &mut ErDiagram, s: &mut ErViewState, events: Vec<Event>| -> (Duration, usize) {
        time += 1.0 / 60.0;
        let mut input = RawInput { screen_rect: Some(screen), time: Some(time), events, ..Default::default() };
        input.viewports.entry(egui::ViewportId::ROOT).or_default().native_pixels_per_point = Some(2.0);
        let t = Instant::now();
        let mut out = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                show_er(ui, d, s);
            });
        });
        let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
        let elapsed = t.elapsed();
        // headless: there is no renderer to upload font textures to
        out.textures_delta.clear();
        let vertices = prims
            .iter()
            .map(|p| match &p.primitive {
                egui::epaint::Primitive::Mesh(m) => m.vertices.len(),
                _ => 0,
            })
            .sum();
        (elapsed, vertices)
    };

    let mut scenario = |name: &str, zoom: Option<f32>, pan_step: egui::Vec2| {
        let mut state = ErViewState::default();
        // settle (fonts, first layout passes)
        for _ in 0..5 {
            frame(&ctx, &mut diagram, &mut state, vec![]);
        }
        if std::env::var("ER_BENCH_DEBUG").is_ok() {
            println!("after settle: zoom {} pan {:?} bounds {:?}", state.zoom, state.pan, diagram.bounds());
        }
        if let Some(z) = zoom {
            state.zoom = z;
            state.pan = vec2(-200.0, -200.0);
        }
        let mut samples = Vec::new();
        let mut verts = 0;
        for i in 0..120 {
            state.pan += pan_step;
            let hover = Pos2::new(300.0 + (i % 40) as f32 * 25.0, 300.0 + (i % 17) as f32 * 30.0);
            let (d, v) = frame(&ctx, &mut diagram, &mut state, vec![Event::PointerMoved(hover)]);
            samples.push(d);
            verts = verts.max(v);
        }
        let (avg, p50, p95, max) = stats(&mut samples);
        println!(
            "{name:<34} zoom {:>5.2}: avg {avg:.2} ms, p50 {p50:.2} ms, p95 {p95:.2} ms, max {max:.2} ms, ~{verts} vertices",
            state.zoom
        );
    };
    scenario("fit (overview, panning)", None, vec2(3.0, 1.0));
    scenario("zoom 0.35 (skeleton rows, panning)", Some(0.35), vec2(-12.0, -6.0));
    scenario("zoom 0.6 (all text, panning)", Some(0.6), vec2(-12.0, -6.0));
    scenario("zoom 1.0 (all text, panning)", Some(1.0), vec2(-15.0, -8.0));
}
