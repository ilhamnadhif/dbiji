//! Headless screenshots of the demo UI (visual check without a GPU).
//!
//! Renders frames through `egui::Context::run_ui`, tessellates them and
//! rasterises the meshes on the CPU into PNG files.
//!
//! ```text
//! cargo run --release -p dbiji-grid --example grid_snapshot -- <output dir>
//! ```

#[path = "support/datagen.rs"]
mod datagen;
#[path = "support/demo_app.rs"]
mod demo_app;

use egui::epaint::{ClippedPrimitive, Primitive};
use egui::{Color32, ColorImage, Event, Key, Modifiers, PointerButton, Pos2, RawInput, Rect, TextureId, vec2};
use std::collections::HashMap;

struct Raster {
    textures: HashMap<TextureId, ColorImage>,
}

impl Raster {
    fn apply(&mut self, delta: &egui::TexturesDelta) {
        for (id, deltas) in &delta.set {
            for d in deltas {
                let egui::ImageData::Color(img) = &d.image;
                match d.pos {
                    None => {
                        self.textures.insert(*id, (**img).clone());
                    }
                    Some([x, y]) => {
                        if let Some(t) = self.textures.get_mut(id) {
                            for row in 0..img.size[1] {
                                for col in 0..img.size[0] {
                                    let (tx, ty) = (x + col, y + row);
                                    if tx < t.size[0] && ty < t.size[1] {
                                        t.pixels[ty * t.size[0] + tx] = img.pixels[row * img.size[0] + col];
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn render(&self, prims: &[ClippedPrimitive], size: [usize; 2], ppp: f32, bg: Color32) -> image::RgbaImage {
        let (w, h) = (size[0], size[1]);
        let mut fb: Vec<[f32; 4]> = vec![[bg.r() as f32, bg.g() as f32, bg.b() as f32, 255.0]; w * h];
        for p in prims {
            let Primitive::Mesh(mesh) = &p.primitive else { continue };
            let Some(tex) = self.textures.get(&mesh.texture_id) else { continue };
            let clip = p.clip_rect;
            let (cx0, cy0) = ((clip.min.x * ppp).max(0.0) as i64, (clip.min.y * ppp).max(0.0) as i64);
            let (cx1, cy1) = (((clip.max.x * ppp).ceil() as i64).min(w as i64), ((clip.max.y * ppp).ceil() as i64).min(h as i64));
            for tri in mesh.indices.as_chunks::<3>().0 {
                let v: Vec<_> = tri.iter().map(|&i| mesh.vertices[i as usize]).collect();
                let pts: Vec<(f32, f32)> = v.iter().map(|v| (v.pos.x * ppp, v.pos.y * ppp)).collect();
                let minx = pts.iter().map(|p| p.0).fold(f32::INFINITY, f32::min).floor().max(cx0 as f32) as i64;
                let maxx = pts.iter().map(|p| p.0).fold(f32::NEG_INFINITY, f32::max).ceil().min(cx1 as f32) as i64;
                let miny = pts.iter().map(|p| p.1).fold(f32::INFINITY, f32::min).floor().max(cy0 as f32) as i64;
                let maxy = pts.iter().map(|p| p.1).fold(f32::NEG_INFINITY, f32::max).ceil().min(cy1 as f32) as i64;
                let area = (pts[1].0 - pts[0].0) * (pts[2].1 - pts[0].1) - (pts[2].0 - pts[0].0) * (pts[1].1 - pts[0].1);
                if area.abs() < 1e-9 {
                    continue;
                }
                for y in miny..maxy {
                    for x in minx..maxx {
                        let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                        let w0 = ((pts[1].0 - px) * (pts[2].1 - py) - (pts[2].0 - px) * (pts[1].1 - py)) / area;
                        let w1 = ((pts[2].0 - px) * (pts[0].1 - py) - (pts[0].0 - px) * (pts[2].1 - py)) / area;
                        let w2 = 1.0 - w0 - w1;
                        if w0 < -1e-4 || w1 < -1e-4 || w2 < -1e-4 {
                            continue;
                        }
                        let u = w0 * v[0].uv.x + w1 * v[1].uv.x + w2 * v[2].uv.x;
                        let vv = w0 * v[0].uv.y + w1 * v[1].uv.y + w2 * v[2].uv.y;
                        let tx = ((u * tex.size[0] as f32) as usize).min(tex.size[0] - 1);
                        let ty = ((vv * tex.size[1] as f32) as usize).min(tex.size[1] - 1);
                        let t = tex.pixels[ty * tex.size[0] + tx];
                        let mut c = [0f32; 4];
                        for (k, ch) in c.iter_mut().enumerate() {
                            let vc = w0 * v[0].color[k] as f32 + w1 * v[1].color[k] as f32 + w2 * v[2].color[k] as f32;
                            *ch = vc * t[k] as f32 / 255.0;
                        }
                        let d = &mut fb[y as usize * w + x as usize];
                        let a = c[3] / 255.0;
                        for k in 0..4 {
                            d[k] = c[k] + d[k] * (1.0 - a);
                        }
                    }
                }
            }
        }
        let mut img = image::RgbaImage::new(w as u32, h as u32);
        for (i, px) in fb.iter().enumerate() {
            img.put_pixel((i % w) as u32, (i / w) as u32, image::Rgba([px[0] as u8, px[1] as u8, px[2] as u8, 255]));
        }
        img
    }
}

struct Shot {
    ctx: egui::Context,
    raster: Raster,
    app: demo_app::DemoApp,
    size: egui::Vec2,
    ppp: f32,
    time: f64,
}

impl Shot {
    fn frame(&mut self, events: Vec<Event>) -> Vec<ClippedPrimitive> {
        self.time += 0.5;
        let mut viewports = egui::ViewportIdMap::default();
        viewports.insert(egui::ViewportId::ROOT, egui::ViewportInfo { native_pixels_per_point: Some(self.ppp), focused: Some(true), ..Default::default() });
        let input = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, self.size)), time: Some(self.time), events, focused: true, viewports, ..Default::default() };
        let app = &mut self.app;
        let mut out = self.ctx.run_ui(input, |ui| app.ui(ui));
        self.raster.apply(&out.textures_delta);
        out.textures_delta.clear();
        let shapes = std::mem::take(&mut out.shapes);
        self.ctx.tessellate(shapes, out.pixels_per_point)
    }

    fn save(&mut self, path: &str, events: Vec<Event>) {
        // Two frames so layouts settle (sizing passes, popups).
        let _ = self.frame(events);
        let prims = self.frame(vec![]);
        let bg = self.ctx.global_style().visuals.panel_fill;
        let size = [(self.size.x * self.ppp) as usize, (self.size.y * self.ppp) as usize];
        let img = self.raster.render(&prims, size, self.ppp, bg);
        if let Err(e) = img.save(path) {
            eprintln!("cannot save {path}: {e}");
        } else {
            println!("wrote {path}");
        }
    }
}

fn click(p: Pos2, button: PointerButton, mods: Modifiers) -> Vec<Event> {
    vec![Event::PointerMoved(p), Event::PointerButton { pos: p, button, pressed: true, modifiers: mods }, Event::PointerButton { pos: p, button, pressed: false, modifiers: mods }]
}

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let ctx = egui::Context::default();
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);
    ctx.set_theme(egui::Theme::Dark);
    let mut s = Shot { ctx, raster: Raster { textures: HashMap::new() }, app: demo_app::DemoApp::new(100_000, 40, 3_000), size: vec2(1400.0, 820.0), ppp: 1.0, time: 0.0 };
    let _ = s.frame(vec![]);
    // Some pending edits so the change colours show.
    let _ = s.app.model.set_cell(2, 1, "Edited Name");
    let _ = s.app.model.set_cell(3, 2, "1234.56");
    let _ = s.app.model.add_row(Some(4));
    let _ = s.app.model.delete_rows(&[7]);
    s.app.view.grid.selection_mut().select_cell(dbiji_grid::grid::CellPos::new(9, 1));
    s.app.view.grid.selection_mut().extend_to(dbiji_grid::grid::CellPos::new(12, 4));
    s.save(&format!("{dir}/grid_dark.png"), vec![]);
    s.app.view.open_panel(dbiji_grid::PanelKind::Calc);
    s.save(&format!("{dir}/grid_calc_panel.png"), vec![]);
    s.app.view.grid.select_cell(1, 12);
    s.app.view.open_panel(dbiji_grid::PanelKind::Value);
    s.save(&format!("{dir}/grid_value_json.png"), vec![]);
    s.app.view.value.mode = dbiji_grid::panels::ValueMode::JsonTree;
    s.save(&format!("{dir}/grid_value_json_tree.png"), vec![]);
    s.app.view.value.mode = dbiji_grid::panels::ValueMode::Auto;
    s.app.view.grid.select_cell(0, 18);
    s.save(&format!("{dir}/grid_value_image.png"), vec![]);
    s.app.view.value.mode = dbiji_grid::panels::ValueMode::Hex;
    s.save(&format!("{dir}/grid_value_hex.png"), vec![]);
    s.app.view.value.mode = dbiji_grid::panels::ValueMode::Auto;
    s.app.view.open_panel(dbiji_grid::PanelKind::Metadata);
    s.save(&format!("{dir}/grid_metadata.png"), vec![]);
    s.ctx.set_theme(egui::Theme::Light);
    s.app.view.toolbar.open_panels.clear();
    s.save(&format!("{dir}/grid_light.png"), vec![]);
    // Context menu on a cell.
    s.save(&format!("{dir}/grid_menu.png"), click(Pos2::new(300.0, 260.0), PointerButton::Secondary, Modifiers::NONE));
    s.save(&format!("{dir}/grid_menu_closed.png"), vec![Event::Key { key: Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }]);
    s.app.view.toolbar.view = dbiji_grid::ResultViewKind::Record;
    s.save(&format!("{dir}/record_view.png"), vec![]);
    s.app.view.toolbar.view = dbiji_grid::ResultViewKind::Text;
    s.save(&format!("{dir}/text_view.png"), vec![]);
    s.app.view.toolbar.view = dbiji_grid::ResultViewKind::Json;
    s.save(&format!("{dir}/json_view.png"), vec![]);
    s.app.view.toolbar.view = dbiji_grid::ResultViewKind::Grid;
    s.ppp = 2.0;
    s.size = vec2(700.0, 420.0);
    s.save(&format!("{dir}/grid_retina.png"), vec![]);
}
