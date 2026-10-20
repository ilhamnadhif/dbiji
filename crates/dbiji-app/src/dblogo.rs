//! Database logos ([Simple Icons](https://simpleicons.org), CC0): connection
//! icons in the navigator, the connection dialog and the start page, drawn
//! in the database's colour. The SVGs are rendered once per size as white
//! masks and tinted when painted.

use dbiji_core::DbKind;
use egui::{Color32, Context, Rect, TextureHandle};

fn svg(kind: DbKind) -> Option<&'static str> {
    Some(match kind {
        DbKind::Postgres => include_str!("../assets/db/postgresql.svg"),
        DbKind::MySql => include_str!("../assets/db/mysql.svg"),
        DbKind::MariaDb => include_str!("../assets/db/mariadb.svg"),
        DbKind::Sqlite => include_str!("../assets/db/sqlite.svg"),
        DbKind::CockroachDb => include_str!("../assets/db/cockroachlabs.svg"),
        DbKind::ClickHouse => include_str!("../assets/db/clickhouse.svg"),
        DbKind::DuckDb => include_str!("../assets/db/duckdb.svg"),
        _ => return None,
    })
}

/// The logo as a `px`×`px` white mask (straight alpha), or `None` without one.
fn rasterize(kind: DbKind, px: u32) -> Option<egui::ColorImage> {
    use resvg::{tiny_skia, usvg};
    let tree = usvg::Tree::from_str(svg(kind)?, &usvg::Options::default()).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(px, px)?;
    let size = tree.size();
    let scale = px as f32 / size.width().max(size.height());
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    let rgba: Vec<u8> = pixmap.data().as_chunks::<4>().0.iter().flat_map(|p| [255, 255, 255, p[3]]).collect();
    Some(egui::ColorImage::from_rgba_unmultiplied([px as usize, px as usize], &rgba))
}

/// The logo texture at `px` pixels (cached in the context).
fn texture(ctx: &Context, kind: DbKind, px: u32) -> Option<TextureHandle> {
    let id = egui::Id::new(("dbiji_db_logo", kind, px));
    if let Some(t) = ctx.data(|d| d.get_temp::<Option<TextureHandle>>(id)) {
        return t;
    }
    let t = rasterize(kind, px).map(|img| ctx.load_texture(format!("db-logo-{kind:?}-{px}"), img, egui::TextureOptions::LINEAR));
    ctx.data_mut(|d| d.insert_temp(id, t.clone()));
    t
}

/// Pixels for a logo `points` wide (steps of 8 to share textures).
fn pixels(ctx: &Context, points: f32) -> u32 {
    ((points * ctx.pixels_per_point()).ceil() as u32).next_multiple_of(8).clamp(16, 128)
}

/// Paint the logo of `kind` in `rect` in `color`; false when it has none.
pub fn paint(ui: &egui::Ui, kind: DbKind, rect: Rect, color: Color32) -> bool {
    let Some(tex) = texture(ui.ctx(), kind, pixels(ui.ctx(), rect.width().max(rect.height()))) else { return false };
    ui.painter().image(tex.id(), rect, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), color);
    true
}

/// The logo as an image `points` square (for buttons and labels).
pub fn image(ctx: &Context, kind: DbKind, points: f32, color: Color32) -> Option<egui::Image<'static>> {
    let tex = texture(ctx, kind, pixels(ctx, points))?;
    Some(egui::Image::new((tex.id(), egui::vec2(points, points))).tint(color))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_logo_renders() {
        for kind in DbKind::ALL {
            match rasterize(kind, 32) {
                Some(img) => assert!(img.pixels.iter().filter(|p| p.a() > 0).count() > 50, "{kind:?} is empty"),
                None => assert_eq!(kind, DbKind::SqlServer, "{kind:?} has no logo"),
            }
        }
    }
}
