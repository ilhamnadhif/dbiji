//! Render the editor headlessly to PNG files (visual QA without a display).
//!
//! ```sh
//! cargo run -p dbiji-editor --example render_snapshot -- target/editor
//! ```
//!
//! Writes `<prefix>_dark.png` and `<prefix>_light.png`: the editor with a
//! selection, diagnostics, statement highlight, spelling issue, find bar and
//! open completion popup. The tessellated egui meshes are rasterized on the
//! CPU (premultiplied alpha, nearest-neighbour texture sampling), which is
//! accurate enough to check layout and colors.

use std::collections::HashMap;

use dbiji_editor::{
    BasicSqlLanguage, Diagnostic, EditorOptions, EditorState, EditorTheme, Severity, SqlEditor,
    TableInfo,
};
use egui::epaint::{ClippedPrimitive, ImageData, Primitive, TextureId};
use egui::{Color32, ColorImage, Event, Pos2, RawInput, Rect, vec2};

const W: usize = 1000;
const H: usize = 560;

struct Canvas {
    px: Vec<[f32; 4]>,
}

fn load(c: Color32) -> [f32; 4] {
    [
        c.r() as f32 / 255.0,
        c.g() as f32 / 255.0,
        c.b() as f32 / 255.0,
        c.a() as f32 / 255.0,
    ]
}

fn sample(tex: &ColorImage, u: f32, v: f32) -> [f32; 4] {
    let x = ((u * tex.size[0] as f32) as usize).min(tex.size[0].saturating_sub(1));
    let y = ((v * tex.size[1] as f32) as usize).min(tex.size[1].saturating_sub(1));
    tex.pixels
        .get(y * tex.size[0] + x)
        .map_or([1.0; 4], |c| load(*c))
}

impl Canvas {
    fn draw(&mut self, prims: &[ClippedPrimitive], textures: &HashMap<TextureId, ColorImage>) {
        for p in prims {
            let Primitive::Mesh(mesh) = &p.primitive else {
                continue;
            };
            let Some(tex) = textures.get(&mesh.texture_id) else {
                continue;
            };
            let clip = p.clip_rect;
            for tri in mesh.indices.as_chunks::<3>().0 {
                let v = [tri[0], tri[1], tri[2]].map(|i| mesh.vertices[i as usize]);
                let edge = |a: Pos2, b: Pos2, c: Pos2| {
                    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
                };
                let area = edge(v[0].pos, v[1].pos, v[2].pos);
                if area.abs() < 1e-6 {
                    continue;
                }
                let min_x = v
                    .iter()
                    .map(|v| v.pos.x)
                    .fold(f32::INFINITY, f32::min)
                    .max(clip.min.x)
                    .max(0.0) as usize;
                let max_x = v
                    .iter()
                    .map(|v| v.pos.x)
                    .fold(f32::NEG_INFINITY, f32::max)
                    .min(clip.max.x)
                    .min(W as f32) as usize;
                let min_y = v
                    .iter()
                    .map(|v| v.pos.y)
                    .fold(f32::INFINITY, f32::min)
                    .max(clip.min.y)
                    .max(0.0) as usize;
                let max_y = v
                    .iter()
                    .map(|v| v.pos.y)
                    .fold(f32::NEG_INFINITY, f32::max)
                    .min(clip.max.y)
                    .min(H as f32) as usize;
                for y in min_y..max_y.min(H) {
                    for x in min_x..max_x.min(W) {
                        let p = Pos2::new(x as f32 + 0.5, y as f32 + 0.5);
                        let w0 = edge(v[1].pos, v[2].pos, p) / area;
                        let w1 = edge(v[2].pos, v[0].pos, p) / area;
                        let w2 = edge(v[0].pos, v[1].pos, p) / area;
                        if w0 < -1e-4 || w1 < -1e-4 || w2 < -1e-4 {
                            continue;
                        }
                        let u = w0 * v[0].uv.x + w1 * v[1].uv.x + w2 * v[2].uv.x;
                        let t = w0 * v[0].uv.y + w1 * v[1].uv.y + w2 * v[2].uv.y;
                        let c = [load(v[0].color), load(v[1].color), load(v[2].color)];
                        let texel = sample(tex, u, t);
                        let mut src = [0.0f32; 4];
                        for k in 0..4 {
                            src[k] = (w0 * c[0][k] + w1 * c[1][k] + w2 * c[2][k]) * texel[k];
                        }
                        let dst = &mut self.px[y * W + x];
                        for k in 0..4 {
                            dst[k] = src[k] + dst[k] * (1.0 - src[3]);
                        }
                    }
                }
            }
        }
    }

    fn save(&self, path: &str) -> Result<(), String> {
        let bytes: Vec<u8> = self
            .px
            .iter()
            .flat_map(|p| {
                let a = p[3].clamp(0.0, 1.0);
                let un = |c: f32| {
                    if a > 0.0 {
                        (c / a).clamp(0.0, 1.0)
                    } else {
                        0.0
                    }
                };
                [un(p[0]), un(p[1]), un(p[2]), 1.0].map(|c| (c * 255.0).round() as u8)
            })
            .collect();
        let img = image::RgbaImage::from_raw(W as u32, H as u32, bytes).ok_or("bad buffer")?;
        img.save(path).map_err(|e| e.to_string())
    }
}

fn render_wrap(path: &str) -> Result<(), String> {
    let ctx = egui::Context::default();
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);
    ctx.set_visuals(egui::Visuals::dark());
    let lang = BasicSqlLanguage::new();
    let text = "SELECT coalesce(a.first_name || ' ' || a.last_name, 'unknown') AS full_name, count(*) FILTER (WHERE b.status = 'open') AS open_items, max(b.updated_at) AS last_update FROM accounts a JOIN items b ON b.account_id = a.id GROUP BY 1;\n-- second line with emoji 👍 and 中文 text\nSELECT (1 + (2 * 3));";
    let mut state = EditorState::new("wrap", text.to_owned());
    let options = EditorOptions {
        cursor_blink: false,
        word_wrap: true,
        ..EditorOptions::default()
    };
    let theme = EditorTheme::dark();
    let mut textures: HashMap<TextureId, ColorImage> = HashMap::new();
    let mut canvas = Canvas {
        px: vec![[0.0; 4]; W * H],
    };
    for frame in 0..30 {
        if frame == 1 {
            state.request_focus(&ctx);
            let s = text.find("count").unwrap_or(0);
            let e = text.find("-- second").map_or(0, |p| p + 9);
            state.select(&ctx, s..e);
        }
        if frame == 20 && std::env::var_os("SNAPSHOT_KEEP_SELECTION").is_none() {
            // Caret right after the inner `(` of `(2 * 3)`.
            let p = text.rfind("(2").map_or(0, |p| p + 1);
            state.set_cursor(&ctx, p);
        }
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(620.0, H as f32))),
            time: Some(frame as f64 / 60.0),
            focused: true,
            ..Default::default()
        };
        let out = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let _ = SqlEditor::new(&mut state, &lang, &options, &theme).show(ui);
            });
        });
        apply_textures(&mut textures, &out.textures_delta);
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        if frame == 29 {
            canvas.draw(&prims, &textures);
        }
        let mut delta = out.textures_delta;
        delta.clear();
    }
    canvas.save(path)
}

fn apply_textures(textures: &mut HashMap<TextureId, ColorImage>, delta: &egui::TexturesDelta) {
    for (id, deltas) in &delta.set {
        for delta in deltas {
            let ImageData::Color(img) = &delta.image;
            match delta.pos {
                None => {
                    textures.insert(*id, (**img).clone());
                }
                Some([x0, y0]) => {
                    if let Some(t) = textures.get_mut(id) {
                        for y in 0..img.size[1] {
                            for x in 0..img.size[0] {
                                let (tx, ty) = (x0 + x, y0 + y);
                                if tx < t.size[0] && ty < t.size[1] {
                                    t.pixels[ty * t.size[0] + tx] = img.pixels[y * img.size[0] + x];
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn render(dark: bool, path: &str) -> Result<(), String> {
    let ctx = egui::Context::default();
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);
    ctx.set_visuals(if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    });
    let lang = BasicSqlLanguage::new().with_tables(vec![
        TableInfo {
            name: "users".into(),
            columns: vec![
                "id".into(),
                "name".into(),
                "email".into(),
                "created_at".into(),
            ],
        },
        TableInfo {
            name: "orders".into(),
            columns: vec!["id".into(), "user_id".into(), "total".into()],
        },
    ]);
    let text = "-- Monthly report: teh numbers\nSELECT u.id, u.name, count(o.id) AS orders\nFROM users u\nLEFT JOIN orders o ON o.user_id = u.id\nWHERE u.created_at >= '2026-01-01'\n  AND u.email LIKE '%@example.com'\nGROUP BY u.id, u.name\nORDER BY orders DESC;\n\n/* totals */\nselect x frm orders WHERE total > 100;\nSELECT u.";
    let mut state = EditorState::new("snapshot", text.to_owned());
    let options = EditorOptions {
        cursor_blink: false,
        ..EditorOptions::default()
    };
    let mut textures: HashMap<TextureId, ColorImage> = HashMap::new();
    let mut canvas = Canvas {
        px: vec![[0.0; 4]; W * H],
    };
    let mut time = 0.0;
    for frame in 0..8 {
        time += 1.0 / 60.0;
        if frame == 1 {
            state.request_focus(&ctx);
            let p = text.find("orders o").unwrap_or(0);
            state.select(&ctx, p..p + 8);
            let x = text.find("select x").unwrap_or(0);
            state.set_diagnostics(vec![
                Diagnostic::new(x + 7..x + 8, Severity::Error, "unknown column x"),
                Diagnostic::new(x + 9..x + 12, Severity::Warning, "did you mean FROM?"),
            ]);
            let s = text.find("SELECT").unwrap_or(0);
            let e = text.find("DESC;").map_or(text.len(), |p| p + 5);
            state.set_statement_highlight(Some(s..e));
            let teh = text.find("teh").unwrap_or(0);
            state.set_spell_issues(std::iter::once(teh..teh + 3).collect());
            state.open_find(false);
        }
        if frame == 4 {
            state.set_cursor(&ctx, text.len());
            state.request_focus(&ctx);
            state.trigger_completion();
        }
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(W as f32, H as f32))),
            time: Some(time),
            focused: true,
            events: if frame == 2 {
                vec![Event::Text("o".into())]
            } else {
                vec![]
            },
            ..Default::default()
        };
        let theme = EditorTheme::from_visuals(&if dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });
        let out = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let _ = SqlEditor::new(&mut state, &lang, &options, &theme).show(ui);
            });
        });
        apply_textures(&mut textures, &out.textures_delta);
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        if frame == 7 {
            canvas.draw(&prims, &textures);
        }
        let mut delta = out.textures_delta;
        delta.clear();
    }
    canvas.save(path)
}

fn main() {
    let prefix = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "target/editor".to_owned());
    for (dark, name) in [(true, "dark"), (false, "light")] {
        let path = format!("{prefix}_{name}.png");
        match render(dark, &path) {
            Ok(()) => println!("wrote {path}"),
            Err(e) => eprintln!("failed to write {path}: {e}"),
        }
    }
    let path = format!("{prefix}_wrap.png");
    match render_wrap(&path) {
        Ok(()) => println!("wrote {path}"),
        Err(e) => eprintln!("failed to write {path}: {e}"),
    }
}
