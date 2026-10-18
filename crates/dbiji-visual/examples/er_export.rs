//! Writes an ER diagram of a generated schema as SVG and PNG.
//!
//! `cargo run -p dbiji-visual --example er_export -- <out_dir> [tables] [scale]`

#[path = "common/demo_data.rs"]
mod demo_data;

use dbiji_visual::er::{ErDiagram, ErModel, export_png, export_svg};
use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let tables: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(60);
    let scale: f32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(1.0);
    let snapshot = demo_data::schema(tables, 42);
    let model = ErModel::from_snapshot(&snapshot, None);
    let started = std::time::Instant::now();
    let diagram = ErDiagram::new("Generated schema", model);
    println!("layout of {} tables: {:?}", diagram.model.tables.len(), started.elapsed());
    let svg = export_svg(&diagram);
    if let Err(e) = std::fs::write(dir.join("er.svg"), &svg) {
        eprintln!("cannot write SVG: {e}");
        return;
    }
    match export_png(&diagram, scale) {
        Ok(png) => {
            if let Err(e) = std::fs::write(dir.join("er.png"), png) {
                eprintln!("cannot write PNG: {e}");
            }
        }
        Err(e) => eprintln!("PNG export failed: {e}"),
    }
    println!("written to {}", dir.display());
}
