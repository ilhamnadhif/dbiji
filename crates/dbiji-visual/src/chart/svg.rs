//! SVG export of charts (light theme, self-contained).

use super::config::{ChartConfig, ChartKind};
use super::data::{ChartData, XKind};
use super::view::{pie_geometry, polar};
use crate::common::colors;
use crate::common::format::{compact, grouped};
use crate::common::svg::{Anchor, SvgDoc, TextStyle, num};
use crate::common::text::{text_width, truncate_to_width};
use crate::common::ticks::{nice_range, ticks};
use dbiji_core::{DbResult, tr};
use egui::{Color32, Pos2, Rect, Vec2, pos2, vec2};

const SURFACE: Color32 = Color32::from_rgb(0xfc, 0xfc, 0xfb);
const INK: Color32 = Color32::from_rgb(0x0b, 0x0b, 0x0b);
const INK2: Color32 = Color32::from_rgb(0x52, 0x51, 0x4e);
const MUTED: Color32 = Color32::from_rgb(0x89, 0x87, 0x81);
const GRID: Color32 = Color32::from_rgb(0xe1, 0xe0, 0xd9);
const AXIS: Color32 = Color32::from_rgb(0xc3, 0xc2, 0xb7);

fn color_of(i: usize, other: bool) -> Color32 {
    if other {
        colors::other(false)
    } else {
        colors::categorical(i, false)
    }
}

/// Exports a chart as PNG: the SVG of [`export_chart_svg`] rendered with
/// resvg at `scale` pixels per point (2 = retina).
pub fn export_chart_png(
    data: &ChartData,
    config: &ChartConfig,
    size: Vec2,
    scale: f32,
) -> DbResult<Vec<u8>> {
    crate::er::svg_to_png(&export_chart_svg(data, config, size), scale)
}

/// Exports a chart as a self-contained SVG document of `size` (points).
pub fn export_chart_svg(data: &ChartData, config: &ChartConfig, size: Vec2) -> String {
    let size = size.max(vec2(200.0, 150.0));
    let mut doc = SvgDoc::new(size.x, size.y, Some(SURFACE));
    let title = if config.title.is_empty() {
        tr("Chart").to_string()
    } else {
        config.title.clone()
    };
    doc.title(&title);
    let mut top = 12.0;
    if !config.title.is_empty() {
        doc.text(
            size.x * 0.5,
            22.0,
            &config.title,
            TextStyle::new(15.0, INK).bold().anchor(Anchor::Middle),
        );
        top = 34.0;
    }
    if data.error.is_some() || data.is_empty() {
        let msg = data
            .error
            .clone()
            .unwrap_or_else(|| tr("Nothing to plot").to_string());
        doc.text(
            size.x * 0.5,
            size.y * 0.5,
            &msg,
            TextStyle::new(13.0, MUTED).anchor(Anchor::Middle),
        );
        return doc.finish();
    }
    let legend = data.series.len() > 1 || config.kind == ChartKind::Pie;
    let legend_w = if legend {
        (size.x * 0.28).clamp(110.0, 200.0)
    } else {
        0.0
    };
    if config.kind == ChartKind::Pie {
        pie(
            &mut doc,
            data,
            config,
            Rect::from_min_max(
                pos2(16.0, top),
                pos2(size.x - legend_w - 16.0, size.y - 16.0),
            ),
            size,
            legend_w,
        );
        return doc.finish();
    }
    let plot = Rect::from_min_max(
        pos2(64.0, top + 14.0),
        pos2(size.x - 16.0 - legend_w, size.y - 44.0),
    );
    if plot.width() < 20.0 || plot.height() < 20.0 {
        return doc.finish();
    }
    let stacked = config.stacked && config.kind.can_stack() && data.series.len() > 1;
    let layers = if stacked { data.stacked() } else { Vec::new() };

    // ranges
    let (mut y_lo, mut y_hi) = if stacked {
        let mut lo = 0.0f64;
        let mut hi = 0.0f64;
        for (_, l, u) in &layers {
            for v in l.iter().chain(u) {
                lo = lo.min(*v);
                hi = hi.max(*v);
            }
        }
        (lo, hi)
    } else {
        data.y_range().unwrap_or((0.0, 1.0))
    };
    if matches!(config.kind, ChartKind::Bar | ChartKind::Area) {
        y_lo = y_lo.min(0.0);
        y_hi = y_hi.max(0.0);
    }
    let (y_lo, y_hi) = nice_range(y_lo, y_hi, 5);
    let (x_lo, x_hi) = match data.x_kind {
        XKind::Categorical => (-0.5, data.categories.len().max(1) as f64 - 0.5),
        XKind::Index => {
            let (a, b) = data.x_range().unwrap_or((0.0, 1.0));
            (a - 0.5, b + 0.5)
        }
        _ => {
            let (a, b) = data.x_range().unwrap_or((0.0, 1.0));
            let pad = if config.kind == ChartKind::Bar {
                bar_slot(data) * 0.6
            } else {
                (b - a).abs() * 0.02
            };
            if (b - a).abs() < f64::EPSILON {
                (a - 1.0, b + 1.0)
            } else {
                (a - pad, b + pad)
            }
        }
    };
    let sx = |x: f64| plot.left() + ((x - x_lo) / (x_hi - x_lo).max(1e-12)) as f32 * plot.width();
    let sy =
        |y: f64| plot.bottom() - ((y - y_lo) / (y_hi - y_lo).max(1e-12)) as f32 * plot.height();

    // grid and axes
    for t in ticks(y_lo, y_hi, 5) {
        let y = sy(t);
        doc.line(pos2(plot.left(), y), pos2(plot.right(), y), GRID, 1.0);
        doc.text(
            plot.left() - 8.0,
            y + 4.0,
            &compact(t, 2),
            TextStyle::new(11.0, MUTED).anchor(Anchor::End),
        );
    }
    let base_y = sy(0.0f64.clamp(y_lo, y_hi));
    doc.line(
        pos2(plot.left(), base_y),
        pos2(plot.right(), base_y),
        AXIS,
        1.0,
    );
    match data.x_kind {
        XKind::Categorical | XKind::Index => {
            let n = (x_hi - x_lo).round().max(1.0) as usize;
            let max_label = (plot.width() / n as f32 - 6.0).max(12.0);
            let widest = (0..n)
                .map(|i| text_width(&data.format_x(x_lo + 0.5 + i as f64), 11.0, false))
                .fold(0.0, f32::max);
            let every = ((widest + 8.0) / (plot.width() / n as f32)).ceil().max(1.0) as usize;
            for i in (0..n).step_by(every) {
                let x = x_lo + 0.5 + i as f64;
                let label = data.format_x(x);
                let label =
                    truncate_to_width(&label, (max_label * every as f32).max(30.0), 11.0, false);
                doc.text(
                    sx(x),
                    plot.bottom() + 16.0,
                    &label,
                    TextStyle::new(11.0, MUTED).anchor(Anchor::Middle),
                );
            }
        }
        _ => {
            let span = x_hi - x_lo;
            for t in ticks(x_lo, x_hi, 6) {
                doc.text(
                    sx(t),
                    plot.bottom() + 16.0,
                    &data.axis_label_x(t, span),
                    TextStyle::new(11.0, MUTED).anchor(Anchor::Middle),
                );
            }
        }
    }
    doc.text(
        plot.center().x,
        size.y - 10.0,
        &data.x_label,
        TextStyle::new(11.5, INK2).anchor(Anchor::Middle),
    );
    doc.text(
        plot.left() - 56.0,
        plot.top() - 6.0,
        &data.y_label,
        TextStyle::new(11.5, INK2),
    );

    // series
    let n_series = data.series.len().max(1) as f64;
    let slot_px = (bar_slot(data) / (x_hi - x_lo).max(1e-12)) as f32 * plot.width();
    let bar_w = if stacked {
        (slot_px * 0.8).min(28.0)
    } else {
        (slot_px * 0.8 / n_series as f32).min(24.0)
    };
    for (i, s) in data.series.iter().enumerate() {
        let color = color_of(i, s.is_other);
        doc.open_group(Some(&s.name));
        match config.kind {
            ChartKind::Line | ChartKind::Area | ChartKind::Pie => {
                let pts: Vec<Pos2> = if let Some((xs, lower, upper)) = layers.get(i) {
                    let top: Vec<Pos2> = xs
                        .iter()
                        .zip(upper)
                        .map(|(x, y)| pos2(sx(*x), sy(*y)))
                        .collect();
                    let mut poly = top.clone();
                    poly.extend(
                        xs.iter()
                            .zip(lower)
                            .rev()
                            .map(|(x, y)| pos2(sx(*x), sy(*y))),
                    );
                    doc.polygon(&poly, Some(color.gamma_multiply(0.35)), None);
                    top
                } else {
                    s.points.iter().map(|p| pos2(sx(p[0]), sy(p[1]))).collect()
                };
                if config.kind == ChartKind::Area && layers.is_empty() && pts.len() > 1 {
                    let mut poly = pts.clone();
                    if let (Some(last), Some(first)) = (pts.last(), pts.first()) {
                        poly.push(pos2(last.x, base_y));
                        poly.push(pos2(first.x, base_y));
                    }
                    doc.polygon(&poly, Some(color.gamma_multiply(0.12)), None);
                }
                doc.polyline(&pts, color, 2.0, None);
                if config.kind == ChartKind::Line && pts.len() <= 60 {
                    for p in &pts {
                        doc.circle(*p, 4.0, Some(color), Some((SURFACE, 2.0)));
                    }
                }
            }
            ChartKind::Scatter => {
                for p in &s.points {
                    doc.circle(
                        pos2(sx(p[0]), sy(p[1])),
                        4.0,
                        Some(color),
                        Some((SURFACE, 1.5)),
                    );
                }
            }
            ChartKind::Bar => {
                let bars: Vec<(f64, f64, f64)> = if let Some((xs, lower, upper)) = layers.get(i) {
                    xs.iter()
                        .zip(lower.iter().zip(upper))
                        .map(|(x, (l, u))| (*x, *l, *u))
                        .collect()
                } else {
                    s.points.iter().map(|p| (p[0], 0.0, p[1])).collect()
                };
                let offset = if stacked {
                    0.0
                } else {
                    (i as f32 - (n_series as f32 - 1.0) * 0.5) * bar_w
                };
                for (x, l, u) in bars {
                    if (u - l).abs() <= 0.0 {
                        continue;
                    }
                    let cx = sx(x) + offset;
                    let (y0, y1) = (sy(l), sy(u));
                    let w = if stacked { bar_w } else { bar_w - 2.0 }.max(1.0);
                    bar_path(
                        &mut doc,
                        cx - w * 0.5,
                        w,
                        y0,
                        y1,
                        color,
                        !stacked || i + 1 == data.series.len(),
                    );
                }
            }
        }
        doc.close_group();
    }
    if legend {
        legend_svg(
            &mut doc,
            data.series
                .iter()
                .enumerate()
                .map(|(i, s)| (s.name.clone(), color_of(i, s.is_other), None))
                .collect(),
            pos2(size.x - legend_w - 4.0, top + 14.0),
            legend_w,
        );
    }
    doc.finish()
}

fn bar_slot(data: &ChartData) -> f64 {
    match data.x_kind {
        XKind::Categorical | XKind::Index => 1.0,
        _ => {
            let mut xs: Vec<f64> = data
                .series
                .iter()
                .flat_map(|s| s.points.iter().map(|p| p[0]))
                .collect();
            xs.sort_by(f64::total_cmp);
            xs.dedup();
            let gap = xs
                .windows(2)
                .map(|w| w[1] - w[0])
                .filter(|d| *d > 0.0)
                .fold(f64::INFINITY, f64::min);
            if gap.is_finite() { gap } else { 1.0 }
        }
    }
}

/// Bar from `y0` (baseline side) to `y1` (data end) with a 4 px rounded data end.
fn bar_path(doc: &mut SvgDoc, x: f32, w: f32, y0: f32, y1: f32, color: Color32, round: bool) {
    let r = if round {
        4.0f32.min(w * 0.5).min((y1 - y0).abs())
    } else {
        0.0
    };
    let up = y1 < y0;
    let (xl, xr) = (x, x + w);
    let d = if r <= 0.0 {
        format!(
            "M {} {} L {} {} L {} {} L {} {} Z",
            num(xl),
            num(y0),
            num(xl),
            num(y1),
            num(xr),
            num(y1),
            num(xr),
            num(y0)
        )
    } else if up {
        format!(
            "M {} {} L {} {} Q {} {} {} {} L {} {} Q {} {} {} {} L {} {} Z",
            num(xl),
            num(y0),
            num(xl),
            num(y1 + r),
            num(xl),
            num(y1),
            num(xl + r),
            num(y1),
            num(xr - r),
            num(y1),
            num(xr),
            num(y1),
            num(xr),
            num(y1 + r),
            num(xr),
            num(y0)
        )
    } else {
        format!(
            "M {} {} L {} {} Q {} {} {} {} L {} {} Q {} {} {} {} L {} {} Z",
            num(xl),
            num(y0),
            num(xl),
            num(y1 - r),
            num(xl),
            num(y1),
            num(xl + r),
            num(y1),
            num(xr - r),
            num(y1),
            num(xr),
            num(y1),
            num(xr),
            num(y1 - r),
            num(xr),
            num(y0)
        )
    };
    doc.path(&d, Some(color), None, None);
}

fn legend_svg(
    doc: &mut SvgDoc,
    items: Vec<(String, Color32, Option<String>)>,
    at: Pos2,
    width: f32,
) {
    for (i, (name, color, extra)) in items.into_iter().enumerate() {
        let y = at.y + i as f32 * 20.0;
        doc.rect(at.x, y, 10.0, 10.0, 3.0, Some(color), None);
        let text = match extra {
            Some(e) => format!("{name} · {e}"),
            None => name,
        };
        let text = truncate_to_width(&text, width - 18.0, 11.5, false);
        doc.text(at.x + 16.0, y + 9.5, &text, TextStyle::new(11.5, INK));
    }
}

fn pie(
    doc: &mut SvgDoc,
    data: &ChartData,
    config: &ChartConfig,
    area: Rect,
    size: Vec2,
    legend_w: f32,
) {
    let slices = data.pie_slices();
    let total: f64 = slices.iter().map(|s| s.value).sum();
    if slices.is_empty() || total <= 0.0 {
        doc.text(
            size.x * 0.5,
            size.y * 0.5,
            tr("No positive values to show"),
            TextStyle::new(13.0, MUTED).anchor(Anchor::Middle),
        );
        return;
    }
    let r = (area.width().min(area.height()) * 0.5 - 6.0).max(10.0);
    let geo = pie_geometry(&slices, area.center(), r, config.donut);
    for (i, s) in slices.iter().enumerate() {
        let (a0, a1) = geo.angles[i];
        let color = color_of(i, s.is_other);
        let large = if a1 - a0 > std::f32::consts::PI { 1 } else { 0 };
        let full = (a1 - a0) >= std::f32::consts::TAU - 1e-4;
        doc.open_group(Some(&format!(
            "{}: {} ({:.1}%)",
            s.label,
            grouped(s.value),
            s.value / total * 100.0
        )));
        if full {
            doc.circle(geo.center, r, Some(color), None);
            if geo.inner > 0.0 {
                doc.circle(geo.center, geo.inner, Some(SURFACE), None);
            }
        } else {
            let (p0, p1) = (polar(geo.center, a0, r), polar(geo.center, a1, r));
            let d = if geo.inner > 0.0 {
                let (q1, q0) = (
                    polar(geo.center, a1, geo.inner),
                    polar(geo.center, a0, geo.inner),
                );
                format!(
                    "M {} {} A {} {} 0 {large} 1 {} {} L {} {} A {} {} 0 {large} 0 {} {} Z",
                    num(p0.x),
                    num(p0.y),
                    num(r),
                    num(r),
                    num(p1.x),
                    num(p1.y),
                    num(q1.x),
                    num(q1.y),
                    num(geo.inner),
                    num(geo.inner),
                    num(q0.x),
                    num(q0.y)
                )
            } else {
                format!(
                    "M {} {} L {} {} A {} {} 0 {large} 1 {} {} Z",
                    num(geo.center.x),
                    num(geo.center.y),
                    num(p0.x),
                    num(p0.y),
                    num(r),
                    num(r),
                    num(p1.x),
                    num(p1.y)
                )
            };
            doc.path(&d, Some(color), Some((SURFACE, 2.0)), None);
        }
        if a1 - a0 >= 0.32 {
            let mid = (a0 + a1) * 0.5;
            let lr = if geo.inner > 0.0 {
                (geo.inner + r) * 0.5
            } else {
                r * 0.64
            };
            let p = polar(geo.center, mid, lr);
            doc.text(
                p.x,
                p.y + 4.0,
                &format!("{:.0}%", s.value / total * 100.0),
                TextStyle::new(12.0, colors::text_on(color)).anchor(Anchor::Middle),
            );
        }
        doc.close_group();
    }
    if geo.inner > 0.0 {
        doc.text(
            geo.center.x,
            geo.center.y + 4.0,
            &compact(total, 2),
            TextStyle::new(18.0, INK).bold().anchor(Anchor::Middle),
        );
    }
    let items = slices
        .iter()
        .enumerate()
        .map(|(i, s)| {
            (
                s.label.clone(),
                color_of(i, s.is_other),
                Some(format!("{:.1}%", s.value / total * 100.0)),
            )
        })
        .collect();
    legend_svg(
        doc,
        items,
        pos2(size.x - legend_w - 4.0, area.top() + 8.0),
        legend_w,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chart::config::ChartAggregate;
    use crate::common::svg::tests::assert_well_formed;
    use dbiji_core::value::{ColumnMeta, DataKind, Value};

    #[test]
    fn every_kind_exports_well_formed_svg() {
        let columns = vec![
            ColumnMeta::new("cat <&>", "text", DataKind::Text),
            ColumnMeta::new("v", "int", DataKind::Int),
            ColumnMeta::new("w", "float", DataKind::Float),
        ];
        let rows: Vec<Vec<Value>> = (0..30)
            .map(|i| {
                vec![
                    Value::text(format!("c{} \"<>\"", i % 7)),
                    Value::Int(i - 5),
                    Value::Float(i as f64 * 1.5),
                ]
            })
            .collect();
        for kind in ChartKind::ALL {
            for stacked in [false, true] {
                let cfg = ChartConfig {
                    kind,
                    x_column: Some(0),
                    y_columns: vec![1, 2],
                    stacked,
                    aggregate: ChartAggregate::Sum,
                    title: "Sales & <returns>".into(),
                    ..Default::default()
                };
                let data = ChartData::build(&columns, &rows, &cfg);
                let svg = export_chart_svg(&data, &cfg, vec2(640.0, 360.0));
                assert_well_formed(&svg);
                assert!(svg.contains("Sales &amp; &lt;returns&gt;"));
            }
        }
        // numeric x and errors
        let cfg = ChartConfig {
            kind: ChartKind::Line,
            x_column: Some(1),
            y_columns: vec![2],
            ..Default::default()
        };
        assert_well_formed(&export_chart_svg(
            &ChartData::build(&columns, &rows, &cfg),
            &cfg,
            vec2(10.0, 10.0),
        ));
        let bad = ChartConfig {
            y_columns: vec![],
            ..Default::default()
        };
        let svg = export_chart_svg(
            &ChartData::build(&columns, &rows, &bad),
            &bad,
            vec2(300.0, 200.0),
        );
        assert_well_formed(&svg);
    }

    #[test]
    fn png_export_renders_at_scale() {
        let columns = vec![
            ColumnMeta::new("region", "text", DataKind::Text),
            ColumnMeta::new("revenue", "int", DataKind::Int),
        ];
        let rows: Vec<Vec<Value>> = ["North", "South", "East"]
            .iter()
            .enumerate()
            .map(|(i, r)| vec![Value::text(*r), Value::Int(10 + i as i64)])
            .collect();
        let cfg = ChartConfig {
            kind: ChartKind::Pie,
            x_column: Some(0),
            y_columns: vec![1],
            ..Default::default()
        };
        let data = ChartData::build(&columns, &rows, &cfg);
        let one = export_chart_png(&data, &cfg, vec2(320.0, 200.0), 1.0).unwrap_or_default();
        let two = export_chart_png(&data, &cfg, vec2(320.0, 200.0), 2.0).unwrap_or_default();
        assert!(one.starts_with(&[0x89, b'P', b'N', b'G']));
        assert!(two.starts_with(&[0x89, b'P', b'N', b'G']));
        // PNG IHDR: width and height are the big-endian u32s at bytes 16..24
        let size = |png: &[u8]| -> (u32, u32) {
            let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
            let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
            (w, h)
        };
        assert_eq!(size(&one), (320, 200));
        assert_eq!(size(&two), (640, 400));
    }
}
