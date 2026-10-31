//! Database logos (`assets/db/README.md`): connection icons in the navigator,
//! the connection dialog and the start page. Coloured logos are drawn in
//! their own colours (as in DBeaver); one-colour ones are rendered as white
//! masks and tinted in the database's colour. Rendered once per size.

use dbiji_core::DbKind;
use egui::{Color32, Context, Rect, TextureHandle};

/// The logo's SVG, and whether it has colours of its own.
fn svg(kind: DbKind) -> Option<(&'static str, bool)> {
    Some(match kind {
        DbKind::Postgres => (include_str!("../assets/db/postgresql.svg"), true),
        DbKind::MySql => (include_str!("../assets/db/mysql.svg"), false),
        DbKind::MariaDb => (include_str!("../assets/db/mariadb.svg"), false),
        DbKind::Sqlite => (include_str!("../assets/db/sqlite.svg"), true),
        DbKind::SqlServer => (include_str!("../assets/db/sqlserver.svg"), true),
        DbKind::CockroachDb => (include_str!("../assets/db/cockroachlabs.svg"), false),
        DbKind::ClickHouse => (include_str!("../assets/db/clickhouse.svg"), true),
        DbKind::Oracle => (include_str!("../assets/db/oracle.svg"), false),
        DbKind::Cassandra => (include_str!("../assets/db/cassandra.svg"), false),
        DbKind::Redis => (include_str!("../assets/db/redis.svg"), true),
        DbKind::MongoDb => (include_str!("../assets/db/mongodb.svg"), true),
        DbKind::Elasticsearch => (include_str!("../assets/db/elasticsearch.svg"), false),
        DbKind::Neo4j => (include_str!("../assets/db/neo4j.svg"), false),
        DbKind::Generic => return None,
    })
}

/// True when the logo of `kind` is drawn in its own colours (not tinted).
fn colored(kind: DbKind) -> bool {
    svg(kind).is_some_and(|(_, c)| c)
}

/// The logo as a `px`×`px` image: in its colours, or as a white mask.
fn rasterize(kind: DbKind, px: u32) -> Option<egui::ColorImage> {
    use resvg::{tiny_skia, usvg};
    let (svg, colored) = svg(kind)?;
    let tree = usvg::Tree::from_str(svg, &usvg::Options::default()).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(px, px)?;
    let size = tree.size();
    let scale = px as f32 / size.width().max(size.height());
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    let size = [px as usize, px as usize];
    if colored {
        return Some(egui::ColorImage::from_rgba_premultiplied(size, pixmap.data()));
    }
    let rgba: Vec<u8> = pixmap.data().as_chunks::<4>().0.iter().flat_map(|p| [255, 255, 255, p[3]]).collect();
    Some(egui::ColorImage::from_rgba_unmultiplied(size, &rgba))
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

/// The tint of a logo: none for a coloured one.
fn tint(kind: DbKind, color: Color32) -> Color32 {
    if colored(kind) { Color32::WHITE } else { color }
}

/// Paint the logo of `kind` in `rect` (one-colour logos in `color`); false when it has none.
pub fn paint(ui: &egui::Ui, kind: DbKind, rect: Rect, color: Color32) -> bool {
    let Some(tex) = texture(ui.ctx(), kind, pixels(ui.ctx(), rect.width().max(rect.height()))) else { return false };
    ui.painter().image(tex.id(), rect, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), tint(kind, color));
    true
}

/// The logo as an image `points` square (for buttons and labels).
pub fn image(ctx: &Context, kind: DbKind, points: f32, color: Color32) -> Option<egui::Image<'static>> {
    let tex = texture(ctx, kind, pixels(ctx, points))?;
    Some(egui::Image::new((tex.id(), egui::vec2(points, points))).tint(tint(kind, color)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_logo_renders() {
        for kind in DbKind::ALL {
            let img = rasterize(kind, 32).unwrap_or_else(|| panic!("{kind:?} has no logo"));
            assert!(img.pixels.iter().filter(|p| p.a() > 0).count() > 50, "{kind:?} is empty");
            // Coloured logos keep their colours; masks are white.
            let tinted = img.pixels.iter().any(|p| p.a() > 200 && (p.r() != p.g() || p.g() != p.b()));
            assert_eq!(tinted, colored(kind), "{kind:?}");
        }
        assert!(colored(DbKind::Postgres) && !colored(DbKind::MySql));
    }
}
