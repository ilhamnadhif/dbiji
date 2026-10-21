//! Image export of ER diagrams: self-contained SVG, and PNG rendered from
//! that SVG with resvg (system fonts loaded through fontdb, with the
//! built-in egui fonts as fallback when the system has none).

use super::diagram::{ErDiagram, Notation};
use super::geometry::{
    COMMENT_FONT, COMMENT_HEIGHT, CORNER_RADIUS, HEADER_FONT, HEADER_HEIGHT, PAD_X, ROW_FONT,
    ROW_HEIGHT, SCHEMA_FONT,
};
use super::model::{DiffStatus, ErRelation, ErTable, ErTableKind};
use super::scene::{ErScene, Route};
use crate::common::colors;
use crate::common::svg::{Anchor, SvgDoc, TextStyle};
use crate::common::text::{text_width, truncate_to_width};
use dbiji_core::{DbError, DbResult, tr, trf};
use egui::{Color32, Pos2, Rect, Vec2, pos2, vec2};
use resvg::{tiny_skia, usvg};
use std::sync::{Arc, OnceLock};

const MARGIN: f32 = 24.0;
/// Largest PNG we render (pixels); bigger requests are scaled down.
const MAX_PIXELS: f32 = 64.0e6;
const MAX_SIDE: f32 = 30_000.0;

/// Fixed light palette of exported images (independent of the UI theme).
struct ExportColors {
    background: Color32,
    box_fill: Color32,
    box_stroke: Color32,
    header_table: Color32,
    header_view: Color32,
    text: Color32,
    weak: Color32,
    type_text: Color32,
    pk: Color32,
    fk: Color32,
    line: Color32,
    note_fill: Color32,
    note_stroke: Color32,
}

const EXPORT: ExportColors = ExportColors {
    background: Color32::WHITE,
    box_fill: Color32::WHITE,
    box_stroke: Color32::from_rgb(0xb4, 0xb2, 0xa8),
    header_table: Color32::from_rgb(0xe6, 0xee, 0xf9),
    header_view: Color32::from_rgb(0xee, 0xeb, 0xfb),
    text: Color32::from_rgb(0x0b, 0x0b, 0x0b),
    weak: Color32::from_rgb(0x89, 0x87, 0x81),
    type_text: Color32::from_rgb(0x52, 0x51, 0x4e),
    pk: Color32::from_rgb(0xc9, 0x85, 0x00),
    fk: Color32::from_rgb(0x2a, 0x78, 0xd6),
    line: Color32::from_rgb(0x52, 0x51, 0x4e),
    note_fill: Color32::from_rgb(0xff, 0xf6, 0xc8),
    note_stroke: Color32::from_rgb(0xe3, 0xcd, 0x6c),
};

/// Header fill for a diff status in exports and the light UI theme.
pub(crate) fn status_header(status: Option<DiffStatus>, kind: ErTableKind) -> Color32 {
    let base = if kind == ErTableKind::View {
        EXPORT.header_view
    } else {
        EXPORT.header_table
    };
    match status {
        Some(DiffStatus::Added) => Color32::WHITE.lerp_to_gamma(colors::STATUS_GOOD, 0.22),
        Some(DiffStatus::Removed) => Color32::WHITE.lerp_to_gamma(colors::STATUS_CRITICAL, 0.20),
        Some(DiffStatus::Modified) => Color32::WHITE.lerp_to_gamma(colors::STATUS_WARNING, 0.30),
        Some(DiffStatus::Unchanged) | None => base,
    }
}

/// Exports the diagram as a self-contained SVG document.
pub fn export_svg(diagram: &ErDiagram) -> String {
    let scene = ErScene::build(diagram);
    let mut bounds = scene.bounds();
    for n in &diagram.notes {
        bounds = Some(bounds.map_or(n.rect(), |b| b.union(n.rect())));
    }
    for r in &scene.relations {
        bounds = bounds.map(|b| b.union(r.route.bbox));
    }
    let bounds = bounds.unwrap_or(Rect::from_min_size(Pos2::ZERO, vec2(200.0, 100.0)));
    let offset = vec2(MARGIN - bounds.min.x, MARGIN - bounds.min.y);
    let legend_h = if diagram.model.tables.iter().any(|t| t.status.is_some()) {
        28.0
    } else {
        0.0
    };
    let size = bounds.size() + Vec2::splat(2.0 * MARGIN) + vec2(0.0, legend_h);
    let mut doc = SvgDoc::new(size.x, size.y, Some(EXPORT.background));
    doc.title(if diagram.name.is_empty() {
        tr("ER diagram")
    } else {
        &diagram.name
    });

    for rel in &scene.relations {
        let Some(r) = diagram.model.relations.get(rel.relation) else {
            continue;
        };
        let pts: Vec<Pos2> = rel.route.points.iter().map(|p| *p + offset).collect();
        doc.open_group(Some(&relation_title(r)));
        let dashed = diagram.settings.notation == Notation::CrowsFoot && !r.identifying;
        doc.polyline(&pts, EXPORT.line, 1.3, dashed.then_some("6 4"));
        let route = Route {
            points: pts,
            ..rel.route.clone()
        };
        markers(
            &mut doc,
            &route,
            r,
            diagram.settings.notation,
            unique_child(diagram, r),
        );
        doc.close_group();
    }

    for st in &scene.tables {
        let Some(t) = diagram.model.tables.get(st.table) else {
            continue;
        };
        let rect = st.rect.translate(offset);
        table_box(&mut doc, t, rect, &st.geom, diagram);
    }

    for note in &diagram.notes {
        let rect = note.rect().translate(offset);
        doc.rect(
            rect.left(),
            rect.top(),
            rect.width(),
            rect.height(),
            4.0,
            Some(EXPORT.note_fill),
            Some((EXPORT.note_stroke, 1.0)),
        );
        let style = TextStyle::new(12.0, EXPORT.text);
        let max_lines = ((rect.height() - 8.0) / 16.0).floor().max(1.0) as usize;
        for (i, line) in wrap_lines(&note.text, rect.width() - 16.0, 12.0)
            .into_iter()
            .take(max_lines)
            .enumerate()
        {
            doc.text(
                rect.left() + 8.0,
                rect.top() + 18.0 + i as f32 * 16.0,
                &line,
                style,
            );
        }
    }
    if legend_h > 0.0 {
        let top = size.y - legend_h;
        let mut x = MARGIN;
        for status in [DiffStatus::Added, DiffStatus::Removed, DiffStatus::Modified] {
            let n = diagram
                .model
                .tables
                .iter()
                .filter(|t| t.status == Some(status))
                .count();
            doc.rect(
                x,
                top + 6.0,
                12.0,
                12.0,
                3.0,
                Some(status_header(Some(status), ErTableKind::Table)),
                Some((status_border(status), 1.5)),
            );
            let label = format!("{} ({n})", tr(status.label()));
            doc.text(
                x + 18.0,
                top + 16.5,
                &label,
                TextStyle::new(12.0, EXPORT.text),
            );
            x += 34.0 + text_width(&label, 12.0, false);
        }
    }
    doc.finish()
}

fn status_border(status: DiffStatus) -> Color32 {
    match status {
        DiffStatus::Added => colors::STATUS_GOOD,
        DiffStatus::Removed => colors::STATUS_CRITICAL,
        DiffStatus::Modified => colors::STATUS_SERIOUS,
        DiffStatus::Unchanged => EXPORT.box_stroke,
    }
}

fn relation_title(r: &ErRelation) -> String {
    format!(
        "{}: {}({}) → {}({})",
        r.name,
        r.from_table,
        r.from_columns.join(", "),
        r.to_table,
        r.to_columns.join(", ")
    )
}

/// The child side of `r` is unique (one-to-one relation).
pub(crate) fn unique_child(diagram: &ErDiagram, r: &ErRelation) -> bool {
    let Some(t) = diagram.model.table(&r.from_table) else {
        return false;
    };
    if r.from_columns.len() == 1 {
        return t
            .column(&r.from_columns[0])
            .map(|c| c.unique)
            .unwrap_or(false);
    }
    let pk = t.pk_columns();
    !pk.is_empty()
        && pk.len() == r.from_columns.len()
        && r.from_columns.iter().all(|c| pk.contains(c))
}

fn table_box(
    doc: &mut SvgDoc,
    t: &ErTable,
    rect: Rect,
    geom: &super::geometry::TableGeometry,
    diagram: &ErDiagram,
) {
    let title = match &t.comment {
        Some(c) => format!("{}\n{c}", t.key),
        None => t.key.clone(),
    };
    doc.open_group(Some(&title));
    let header_fill = status_header(t.status, t.kind);
    let stroke_color = match t.status {
        Some(DiffStatus::Added) => colors::STATUS_GOOD,
        Some(DiffStatus::Removed) => colors::STATUS_CRITICAL,
        Some(DiffStatus::Modified) => colors::STATUS_SERIOUS,
        _ => EXPORT.box_stroke,
    };
    doc.rect(
        rect.left(),
        rect.top(),
        rect.width(),
        rect.height(),
        CORNER_RADIUS,
        Some(EXPORT.box_fill),
        None,
    );
    // header band with rounded top corners only
    let hb = Rect::from_min_size(rect.min, vec2(rect.width(), HEADER_HEIGHT));
    let d = format!(
        "M {l} {b} L {l} {t2} Q {l} {t} {l2} {t} L {r2} {t} Q {r} {t} {r} {t2} L {r} {b} Z",
        l = n(hb.left()),
        r = n(hb.right()),
        t = n(hb.top()),
        b = n(hb.bottom()),
        t2 = n(hb.top() + CORNER_RADIUS),
        l2 = n(hb.left() + CORNER_RADIUS),
        r2 = n(hb.right() - CORNER_RADIUS),
    );
    doc.path(&d, Some(header_fill), None, None);
    doc.line(
        pos2(rect.left(), hb.bottom()),
        pos2(rect.right(), hb.bottom()),
        EXPORT.box_stroke,
        1.0,
    );
    doc.rect(
        rect.left(),
        rect.top(),
        rect.width(),
        rect.height(),
        CORNER_RADIUS,
        None,
        Some((stroke_color, 1.2)),
    );

    // kind icon
    let icon_c = pos2(rect.left() + PAD_X + 6.0, hb.center().y);
    if t.kind == ErTableKind::View {
        view_icon(doc, icon_c, EXPORT.type_text);
    } else {
        table_icon(doc, icon_c, EXPORT.type_text);
    }
    let name_x = rect.left() + PAD_X + 18.0;
    let schema_w = t
        .schema
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| text_width(s, SCHEMA_FONT, false) + 10.0)
        .unwrap_or(0.0);
    let name_max = rect.right() - PAD_X - schema_w - name_x;
    let name = truncate_to_width(&t.name, name_max, HEADER_FONT, true);
    doc.text(
        name_x,
        hb.top() + 18.5,
        &name,
        TextStyle::new(HEADER_FONT, EXPORT.text).bold(),
    );
    if let Some(s) = t.schema.as_deref().filter(|s| !s.is_empty()) {
        doc.text(
            rect.right() - PAD_X,
            hb.top() + 18.0,
            s,
            TextStyle::new(SCHEMA_FONT, EXPORT.weak).anchor(Anchor::End),
        );
    }
    if let Some(c) = &geom.comment {
        let c = truncate_to_width(c, rect.width() - 2.0 * PAD_X, COMMENT_FONT, false);
        doc.text(
            rect.left() + PAD_X,
            hb.bottom() + COMMENT_HEIGHT - 5.0,
            &c,
            TextStyle::new(COMMENT_FONT, EXPORT.weak).italic(),
        );
    }
    for (i, &ci) in geom.rows.iter().enumerate() {
        let Some(col) = t.columns.get(ci) else {
            continue;
        };
        let top = rect.top() + geom.row_top(i);
        let cy = top + ROW_HEIGHT * 0.5;
        if col.pk {
            key_icon(doc, pos2(rect.left() + PAD_X, cy), EXPORT.pk);
        } else if col.fk {
            link_icon(doc, pos2(rect.left() + PAD_X, cy), EXPORT.fk);
        }
        let type_space = if diagram.settings.show_types && !col.data_type.is_empty() {
            rect.width() - geom.type_x
        } else {
            0.0
        };
        let name_max = rect.width() - geom.name_x - type_space - 6.0;
        let name = truncate_to_width(&col.name, name_max, ROW_FONT, col.pk);
        let mut style = TextStyle::new(ROW_FONT, EXPORT.text);
        if col.pk {
            style = style.bold();
        }
        doc.text(rect.left() + geom.name_x, top + 14.5, &name, style);
        if diagram.settings.show_types && !col.data_type.is_empty() {
            let ty = truncate_to_width(
                &col.data_type,
                rect.width() - geom.type_x - PAD_X,
                ROW_FONT,
                false,
            );
            doc.text(
                rect.left() + geom.type_x,
                top + 14.5,
                &ty,
                TextStyle::new(ROW_FONT, EXPORT.type_text),
            );
        }
    }
    doc.close_group();
}

fn n(v: f32) -> String {
    crate::common::svg::num(v)
}

fn key_icon(doc: &mut SvgDoc, left: Pos2, c: Color32) {
    let ring = pos2(left.x + 4.0, left.y);
    doc.circle(ring, 3.0, None, Some((c, 1.5)));
    doc.line(
        pos2(ring.x + 3.0, left.y),
        pos2(left.x + 13.0, left.y),
        c,
        1.5,
    );
    doc.line(
        pos2(left.x + 11.0, left.y),
        pos2(left.x + 11.0, left.y + 3.0),
        c,
        1.5,
    );
    doc.line(
        pos2(left.x + 13.0, left.y),
        pos2(left.x + 13.0, left.y + 2.5),
        c,
        1.5,
    );
}

fn link_icon(doc: &mut SvgDoc, left: Pos2, c: Color32) {
    doc.rect(left.x, left.y - 2.5, 8.0, 5.0, 2.5, None, Some((c, 1.4)));
    doc.rect(
        left.x + 5.0,
        left.y - 2.5,
        8.0,
        5.0,
        2.5,
        None,
        Some((c, 1.4)),
    );
}

fn table_icon(doc: &mut SvgDoc, c: Pos2, color: Color32) {
    doc.rect(
        c.x - 6.0,
        c.y - 5.0,
        12.0,
        10.0,
        1.5,
        None,
        Some((color, 1.2)),
    );
    doc.line(
        pos2(c.x - 6.0, c.y - 1.5),
        pos2(c.x + 6.0, c.y - 1.5),
        color,
        1.0,
    );
    doc.line(
        pos2(c.x - 1.0, c.y - 1.5),
        pos2(c.x - 1.0, c.y + 5.0),
        color,
        1.0,
    );
}

fn view_icon(doc: &mut SvgDoc, c: Pos2, color: Color32) {
    let d = format!(
        "M {} {} Q {} {} {} {} Q {} {} {} {} Z",
        n(c.x - 7.0),
        n(c.y),
        n(c.x),
        n(c.y - 7.0),
        n(c.x + 7.0),
        n(c.y),
        n(c.x),
        n(c.y + 7.0),
        n(c.x - 7.0),
        n(c.y)
    );
    doc.path(&d, None, Some((color, 1.2)), None);
    doc.circle(c, 2.0, Some(color), None);
}

/// Cardinality markers at both ends of a route (SVG).
fn markers(
    doc: &mut SvgDoc,
    route: &Route,
    r: &ErRelation,
    notation: Notation,
    unique_child: bool,
) {
    let (Some(&start), Some(&end)) = (route.points.first(), route.points.last()) else {
        return;
    };
    for seg in marker_segments(
        start,
        route.start_dir,
        end,
        route.end_dir,
        r,
        notation,
        unique_child,
    ) {
        match seg {
            MarkerShape::Line(a, b) => doc.line(a, b, EXPORT.line, 1.3),
            MarkerShape::Circle(c, radius) => {
                doc.circle(c, radius, Some(EXPORT.background), Some((EXPORT.line, 1.3)))
            }
            MarkerShape::Arrow(points) => doc.polygon(&points, Some(EXPORT.line), None),
            MarkerShape::Dot(c, radius) => doc.circle(c, radius, Some(EXPORT.line), None),
        }
    }
}

/// Primitive of a relation end marker (diagram units).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MarkerShape {
    Line(Pos2, Pos2),
    /// Hollow circle (center, radius).
    Circle(Pos2, f32),
    /// Filled triangle.
    Arrow(Vec<Pos2>),
    /// Filled dot.
    Dot(Pos2, f32),
}

/// Marker geometry shared by the canvas and the export. `start` is the
/// child end (FK side), `end` the parent end; the direction vectors point
/// away from the boxes.
pub(crate) fn marker_segments(
    start: Pos2,
    start_dir: Vec2,
    end: Pos2,
    end_dir: Vec2,
    r: &ErRelation,
    notation: Notation,
    unique_child: bool,
) -> Vec<MarkerShape> {
    let mut out = Vec::new();
    let perp = |d: Vec2| vec2(-d.y, d.x);
    match notation {
        Notation::CrowsFoot => {
            // child end: zero-or-many (o<) or zero-or-one (o|) when unique
            let n = perp(start_dir);
            if unique_child {
                let bar = start + start_dir * 10.0;
                out.push(MarkerShape::Line(bar - n * 6.0, bar + n * 6.0));
            } else {
                let tip = start + start_dir * 12.0;
                out.push(MarkerShape::Line(tip, start + n * 6.5));
                out.push(MarkerShape::Line(tip, start - n * 6.5));
                out.push(MarkerShape::Line(tip, start));
            }
            out.push(MarkerShape::Circle(start + start_dir * 17.0, 3.5));
            // parent end: exactly one (||) or zero-or-one (o|)
            let n = perp(end_dir);
            let bar1 = end + end_dir * 8.0;
            out.push(MarkerShape::Line(bar1 - n * 6.0, bar1 + n * 6.0));
            if r.optional {
                out.push(MarkerShape::Circle(end + end_dir * 15.5, 3.5));
            } else {
                let bar2 = end + end_dir * 13.0;
                out.push(MarkerShape::Line(bar2 - n * 6.0, bar2 + n * 6.0));
            }
        }
        Notation::Simple => {
            let n = perp(end_dir);
            let base = end + end_dir * 10.0;
            out.push(MarkerShape::Arrow(vec![
                end,
                base + n * 4.5,
                base - n * 4.5,
            ]));
            out.push(MarkerShape::Dot(start + start_dir * 2.5, 2.5));
        }
    }
    out
}

/// Greedy word wrap using the estimated text metrics.
pub(crate) fn wrap_lines(text: &str, max_width: f32, size: f32) -> Vec<String> {
    let mut lines = Vec::new();
    for para in text.lines() {
        let mut line = String::new();
        for word in para.split_whitespace() {
            let candidate = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if text_width(&candidate, size, false) <= max_width || line.is_empty() {
                line = candidate;
            } else {
                lines.push(std::mem::take(&mut line));
                line = word.to_string();
            }
        }
        lines.push(line);
    }
    lines
}

fn font_db() -> Arc<usvg::fontdb::Database> {
    static DB: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        db.load_system_fonts();
        if db.is_empty() {
            // Headless systems without fonts: use the fonts bundled with egui.
            for data in [
                epaint_default_fonts::HACK_REGULAR,
                epaint_default_fonts::NOTO_EMOJI_REGULAR,
            ] {
                db.load_font_data(data.to_vec());
            }
            let family = db
                .faces()
                .next()
                .and_then(|face| face.families.first().map(|(f, _)| f.clone()));
            if let Some(family) = family {
                db.set_sans_serif_family(family);
            }
        }
        Arc::new(db)
    })
    .clone()
}

/// Renders any SVG document (e.g. from [`export_svg`] or
/// [`crate::chart::export_chart_svg`]) to PNG at `scale` pixels per unit,
/// with system fonts (the bundled egui fonts when the system has none).
/// Very large images are scaled down to at most 64 megapixels.
pub fn svg_to_png(svg: &str, scale: f32) -> DbResult<Vec<u8>> {
    let opts = usvg::Options {
        fontdb: font_db(),
        ..Default::default()
    };
    let tree = usvg::Tree::from_str(svg, &opts)
        .map_err(|e| DbError::internal(trf("SVG rendering failed: {}", &[&e])))?;
    let size = tree.size();
    let mut scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let pixels = size.width() * size.height() * scale * scale;
    if pixels > MAX_PIXELS {
        let reduced = (MAX_PIXELS / (size.width() * size.height())).sqrt();
        tracing::warn!(
            requested = scale,
            used = reduced,
            "PNG export scaled down to stay within the pixel limit"
        );
        scale = reduced;
    }
    let side = size.width().max(size.height()) * scale;
    if side > MAX_SIDE {
        scale *= MAX_SIDE / side;
    }
    let w = (size.width() * scale).ceil().max(1.0) as u32;
    let h = (size.height() * scale).ceil().max(1.0) as u32;
    let mut pixmap = tiny_skia::Pixmap::new(w, h)
        .ok_or_else(|| DbError::internal(trf("Cannot allocate a {}x{} image", &[&w, &h])))?;
    pixmap.fill(tiny_skia::Color::WHITE);
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap
        .encode_png()
        .map_err(|e| DbError::io(trf("PNG encoding failed: {}", &[&e])))
}

/// Exports the diagram as PNG. `scale` is the pixel density (2 = retina);
/// very large diagrams are scaled down to at most 64 megapixels.
pub fn export_png(diagram: &ErDiagram, scale: f32) -> DbResult<Vec<u8>> {
    svg_to_png(&export_svg(diagram), scale)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::svg::tests::assert_well_formed;
    use crate::er::diagram::{ErDiagram, ErNote};
    use crate::er::model::{ErColumn, ErModel, ErTable};

    pub(crate) fn sample_diagram() -> ErDiagram {
        let mut m = ErModel::default();
        m.tables.push(
            ErTable::new(Some("shop"), "customers", ErTableKind::Table)
                .with_column(ErColumn::new("id", "bigint").primary_key())
                .with_column(ErColumn::new("email", "varchar(200)").not_null().unique()),
        );
        let mut orders = ErTable::new(Some("shop"), "orders <&> \"quoted\"", ErTableKind::Table)
            .with_column(ErColumn::new("id", "bigint").primary_key())
            .with_column(ErColumn::new("customer_id", "bigint").foreign_key());
        orders.comment = Some("Orders & invoices".into());
        orders.status = Some(DiffStatus::Modified);
        m.tables.push(orders);
        m.tables.push(
            ErTable::new(Some("shop"), "v_totals", ErTableKind::View)
                .with_column(ErColumn::new("total", "numeric")),
        );
        m.relations.push(ErRelation {
            name: "fk_orders_customer".into(),
            from_table: "shop.orders <&> \"quoted\"".into(),
            from_columns: vec!["customer_id".into()],
            to_table: "shop.customers".into(),
            to_columns: vec!["id".into()],
            identifying: false,
            optional: true,
        });
        let mut d = ErDiagram::new("Shop <test>", m);
        d.settings.show_comments = true;
        d.notes.push(ErNote::new(
            "Remember & check\nthe <totals>",
            Pos2::new(-200.0, -100.0),
        ));
        d
    }

    #[test]
    fn svg_is_well_formed_and_escaped() {
        let d = sample_diagram();
        let svg = export_svg(&d);
        assert_well_formed(&svg);
        assert!(svg.contains("font-family=\"Helvetica, Arial, sans-serif\""));
        assert!(svg.contains("orders &lt;&amp;&gt; &quot;quoted&quot;"));
        assert!(svg.contains("Shop &lt;test&gt;"));
        assert!(!svg.contains("<script"));
        // a diagram diff gets a legend
        assert!(svg.contains("Modified (1)"), "diff legend");
        let mut plain = d.clone();
        plain.model.set_statuses(Default::default());
        assert!(!export_svg(&plain).contains("Modified ("));
        // simple notation and empty diagram
        let mut d2 = d.clone();
        d2.settings.notation = Notation::Simple;
        assert_well_formed(&export_svg(&d2));
        assert_well_formed(&export_svg(&ErDiagram::default()));
    }

    #[test]
    fn png_renders() {
        let d = sample_diagram();
        let png = export_png(&d, 1.0).expect("png");
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
        let png2 = export_png(&d, 2.0).expect("png");
        assert!(png2.len() > png.len());
    }

    #[test]
    fn markers_cover_notations() {
        let r = ErRelation {
            optional: false,
            ..Default::default()
        };
        let m = marker_segments(
            pos2(0.0, 0.0),
            vec2(1.0, 0.0),
            pos2(100.0, 0.0),
            vec2(-1.0, 0.0),
            &r,
            Notation::CrowsFoot,
            false,
        );
        assert_eq!(
            m.iter()
                .filter(|s| matches!(s, MarkerShape::Line(..)))
                .count(),
            5
        );
        let r = ErRelation {
            optional: true,
            ..Default::default()
        };
        let m = marker_segments(
            pos2(0.0, 0.0),
            vec2(1.0, 0.0),
            pos2(100.0, 0.0),
            vec2(-1.0, 0.0),
            &r,
            Notation::CrowsFoot,
            true,
        );
        assert_eq!(
            m.iter()
                .filter(|s| matches!(s, MarkerShape::Circle(..)))
                .count(),
            2
        );
        let m = marker_segments(
            pos2(0.0, 0.0),
            vec2(1.0, 0.0),
            pos2(100.0, 0.0),
            vec2(-1.0, 0.0),
            &r,
            Notation::Simple,
            false,
        );
        assert!(m.iter().any(|s| matches!(s, MarkerShape::Arrow(_))));
        assert_eq!(wrap_lines("aaa bbb ccc", 30.0, 12.0).len(), 3);
    }
}
