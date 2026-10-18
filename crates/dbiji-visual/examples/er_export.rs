//! Writes sample image exports: an ER diagram of a generated schema (SVG and
//! PNG) and one chart per chart type (SVG and PNG).
//!
//! `cargo run -p dbiji-visual --example er_export -- <out_dir> [tables] [scale]`

#[path = "common/demo_data.rs"]
mod demo_data;

use dbiji_visual::chart::{
    ChartAggregate, ChartConfig, ChartData, ChartKind, export_chart_png, export_chart_svg,
};
use dbiji_visual::er::{ErDiagram, ErModel, export_png, export_svg};
use std::path::{Path, PathBuf};

fn write(path: &Path, bytes: &[u8]) {
    if let Err(e) = std::fs::write(path, bytes) {
        eprintln!("cannot write {}: {e}", path.display());
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let tables: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(60);
    let scale: f32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(1.0);

    let snapshot = demo_data::schema(tables, 42);
    let model = ErModel::from_snapshot(&snapshot, None);
    let started = std::time::Instant::now();
    let diagram = ErDiagram::new("Generated schema", model);
    println!(
        "layout of {} tables: {:?}",
        diagram.model.tables.len(),
        started.elapsed()
    );
    write(&dir.join("er.svg"), export_svg(&diagram).as_bytes());
    match export_png(&diagram, scale) {
        Ok(png) => write(&dir.join("er.png"), &png),
        Err(e) => eprintln!("PNG export failed: {e}"),
    }

    let (columns, rows) = demo_data::chart_rows(60, 5);
    let charts = [
        (
            "line",
            ChartConfig {
                kind: ChartKind::Line,
                x_column: Some(0),
                y_columns: vec![3],
                series_column: Some(1),
                aggregate: ChartAggregate::Sum,
                title: "Revenue per region".into(),
                ..Default::default()
            },
        ),
        (
            "bar",
            ChartConfig {
                kind: ChartKind::Bar,
                x_column: Some(1),
                y_columns: vec![3, 4],
                aggregate: ChartAggregate::Avg,
                title: "Average revenue and orders".into(),
                ..Default::default()
            },
        ),
        (
            "area",
            ChartConfig {
                kind: ChartKind::Area,
                x_column: Some(0),
                y_columns: vec![3],
                series_column: Some(2),
                aggregate: ChartAggregate::Sum,
                stacked: true,
                title: "Revenue per channel (stacked)".into(),
                ..Default::default()
            },
        ),
        (
            "scatter",
            ChartConfig {
                kind: ChartKind::Scatter,
                x_column: Some(4),
                y_columns: vec![5],
                series_column: Some(2),
                title: "Basket size vs orders".into(),
                ..Default::default()
            },
        ),
        (
            "pie",
            ChartConfig {
                kind: ChartKind::Pie,
                x_column: Some(1),
                y_columns: vec![3],
                aggregate: ChartAggregate::Sum,
                title: "Revenue share".into(),
                ..Default::default()
            },
        ),
    ];
    for (name, config) in charts {
        let data = ChartData::build(&columns, &rows, &config);
        let size = egui::vec2(720.0, 400.0);
        write(
            &dir.join(format!("chart_{name}.svg")),
            export_chart_svg(&data, &config, size).as_bytes(),
        );
        match export_chart_png(&data, &config, size, scale.max(1.0)) {
            Ok(png) => write(&dir.join(format!("chart_{name}.png")), &png),
            Err(e) => eprintln!("chart PNG export failed: {e}"),
        }
    }
    println!("written to {}", dir.display());
}
