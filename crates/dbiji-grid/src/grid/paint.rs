//! Painting of the grid: body cells, row numbers, header, scrollbars, overlays.

use super::GridAction;
use super::frame::{Geo, GridFrame, PAD, fk_button_rect};

/// A fetch running this long offers Cancel.
pub(crate) const CANCEL_AFTER: std::time::Duration = std::time::Duration::from_secs(1);
use super::input::{MENU_ZONE, SORT_ZONE};
use super::state::BodyCache;
use super::state::Drag;
use super::text::{GlyphCache, Placed, TextBatch, layout_slow, place_slow};
use crate::format::PreparedFormat;
use crate::model::{CellRef, RowState};
use crate::theme::mix;
use crate::valueview::{byte_size_text, detect_image};
use dbiji_core::value::BoolStyle;
use dbiji_core::{DataKind, Value, tr, trf};
use egui::text::{LayoutJob, TextWrapping};
use egui::{Align, Align2, Color32, CornerRadius, FontId, Painter, Pos2, Rect, Shape, Stroke, StrokeKind, TextFormat, Ui, pos2, vec2};

/// Per visible column data prepared once per frame.
struct ColInfo<'o> {
    pos: usize,
    col: usize,
    x0: f32,
    x1: f32,
    kind: DataKind,
    is_virtual: bool,
    fk: bool,
    dict: bool,
    fmt: PreparedFormat<'o>,
}

/// A line drawn over text after the text batch (strike-through, underline).
struct Line {
    x0: f32,
    x1: f32,
    y: f32,
    color: Color32,
}

/// Collects the body shapes so a frame can be repeated when the row store
/// is busy (see [`BodyCache`](super::state::BodyCache)).
struct Sink<'p> {
    painter: &'p Painter,
    shapes: Vec<Shape>,
}

impl Sink<'_> {
    fn rect_filled(&mut self, rect: Rect, corner_radius: impl Into<CornerRadius>, color: Color32) {
        self.shapes.push(Shape::rect_filled(rect, corner_radius, color));
    }

    fn rect_stroke(&mut self, rect: Rect, corner_radius: impl Into<CornerRadius>, stroke: Stroke, kind: StrokeKind) {
        self.shapes.push(Shape::rect_stroke(rect, corner_radius, stroke, kind));
    }

    fn line_segment(&mut self, points: [Pos2; 2], stroke: Stroke) {
        self.shapes.push(Shape::line_segment(points, stroke));
    }

    fn hline(&mut self, x: impl Into<egui::Rangef>, y: f32, stroke: Stroke) {
        self.shapes.push(Shape::hline(x, y, stroke));
    }

    fn vline(&mut self, x: f32, y: impl Into<egui::Rangef>, stroke: Stroke) {
        self.shapes.push(Shape::vline(x, y, stroke));
    }

    fn galley(&mut self, pos: Pos2, galley: std::sync::Arc<egui::Galley>, color: Color32) {
        self.shapes.push(Shape::galley(pos, galley, color));
    }
}

/// How a cell is drawn.
struct CellLook<'v> {
    text: std::borrow::Cow<'v, str>,
    color: Color32,
    align: Align,
    badge: Option<&'static str>,
    suffix: Option<&'v str>,
    checkbox: Option<bool>,
}

/// Badge label for special values.
fn badge_for(v: &Value, kind: DataKind) -> Option<&'static str> {
    match v {
        Value::Bytes(b) => Some(if detect_image(b).is_some() { "IMG" } else { "BIN" }),
        Value::Json(_) => Some("JSON"),
        Value::Array(_) => Some("ARR"),
        _ => match kind {
            DataKind::Json => Some("JSON"),
            DataKind::Xml => Some("XML"),
            DataKind::Geometry => Some("GEO"),
            DataKind::Binary => Some("BIN"),
            _ => None,
        },
    }
}

fn is_number(v: &Value) -> bool {
    matches!(v, Value::Int(_) | Value::UInt(_) | Value::Float(_) | Value::Decimal(_))
}

impl GridFrame<'_> {
    /// Paint everything.
    pub fn paint(&mut self, ui: &mut Ui) {
        let geo = self.geo.clone();
        let painter = ui.painter_at(geo.rect);
        painter.rect_filled(geo.rect, 0.0, self.theme.bg);
        self.paint_body(ui, &geo);
        self.paint_rownums(ui, &geo);
        self.paint_header(ui, &geo);
        self.paint_corner(ui, &geo);
        self.paint_scrollbars(ui, &geo);
        self.paint_overlays(ui, &geo);
        // Outer border.
        painter.rect_stroke(geo.rect, 0.0, Stroke::new(1.0, self.theme.line), StrokeKind::Inside);
        if self.focused {
            let c = mix(self.theme.active_border, self.theme.bg, 0.4);
            painter.rect_stroke(geo.rect, 0.0, Stroke::new(1.0, c), StrokeKind::Inside);
        }
    }

    fn paint_body(&mut self, ui: &Ui, geo: &Geo) {
        let painter = ui.painter_at(geo.body);
        let theme = self.theme;
        let (sx, sy) = (self.state.rt.scroll_x, self.state.rt.scroll_y);
        let rows = geo.visible_rows(sy);
        let cols = geo.visible_cols(sx);
        if geo.nrows == 0 || geo.cols.is_empty() {
            let msg = if geo.cols.is_empty() && self.model.column_count() > 0 {
                tr("All columns are hidden")
            } else if self.model.fetch.fetching {
                // With a spinner and, when it takes long, Cancel (see `fetching_overlay`).
                return;
            } else {
                tr("No rows")
            };
            painter.text(geo.body.center(), Align2::CENTER_CENTER, msg, self.font.clone(), theme.weak);
            return;
        }
        let model = &*self.model;
        let options = self.options;
        let font = self.font.clone();
        let badge_font = FontId::new((font.size * 0.72).round().max(8.0), font.family.clone());
        let fmt = &options.format;
        let infos: Vec<ColInfo<'_>> = cols
            .clone()
            .map(|pos| {
                let col = geo.cols[pos];
                ColInfo {
                    pos,
                    col,
                    x0: geo.col_left(pos, sx),
                    x1: geo.col_left(pos + 1, sx),
                    kind: model.column_kind(col),
                    is_virtual: model.is_virtual_column(col),
                    fk: model.foreign_key_for_column(col).is_some(),
                    dict: options.show_descriptions && model.dictionary(col).is_some(),
                    fmt: PreparedFormat::new(fmt, self.state.columns.get(col).and_then(|c| c.format.as_ref())),
                }
            })
            .collect();
        let null_text = fmt.null_text.as_str();
        let default_text = tr("[DEFAULT]");
        let checkbox_style = fmt.bool_style == BoolStyle::Checkbox;
        let selection = &self.state.rt.selection;
        let ranges = selection.ranges();
        let glyphs: &mut GlyphCache = &mut self.state.rt.glyphs;
        let mut batch = TextBatch::new();
        let mut sink = Sink { painter: &painter, shapes: Vec::with_capacity(1024) };
        let mut lines: Vec<Line> = Vec::new();
        let hover = self.hover_cell;
        let command_down = ui.input(|i| i.modifiers.command);
        let pointer = hover.and(ui.input(|i| i.pointer.hover_pos()));
        let fk_icon = |color| painter.layout_no_wrap(egui_phosphor::regular::ARROW_SQUARE_OUT.to_string(), FontId::new(font.size * 0.85, font.family.clone()), color);
        let (fk_idle, fk_hot) = (fk_icon(theme.weak), fk_icon(theme.link));
        let mut tooltip: Option<(Rect, String)> = None;
        let max_tip = options.max_tooltip_chars;
        let striped = options.striped;
        let body = geo.body;
        let row_h = geo.row_h;
        let content_right = geo.col_left(geo.cols.len(), sx).min(body.right());

        let names = model.column_names();
        let highlights: Vec<(usize, Option<String>, egui::Color32)> = self
            .state
            .highlights
            .iter()
            .filter_map(|h| {
                let col = names.iter().position(|n| *n == h.column)?;
                Some((col, h.value.clone(), egui::Color32::from_rgba_unmultiplied(h.color[0], h.color[1], h.color[2], 70)))
            })
            .collect();
        let fresh = model.visit_rows_for(rows.clone(), std::time::Duration::from_millis(6), |row, cells| {
            let y0 = geo.row_top(row, sy);
            let row_rect = Rect::from_min_max(pos2(body.left(), y0), pos2(content_right, y0 + row_h));
            let state = cells.state();
            let highlight = highlights
                .iter()
                .find(|(c, v, _)| {
                    let cell = cells.value_of(*c);
                    match v {
                        None => cell.as_ref().is_none_or(|x| x.is_null()),
                        Some(t) => cell.as_ref().is_some_and(|x| !x.is_null() && x.to_text() == t.as_str()),
                    }
                })
                .map(|h| h.2);
            let row_bg = match state {
                RowState::Inserted => Some(theme.inserted),
                RowState::Deleted => Some(theme.deleted),
                _ if highlight.is_some() => highlight,
                _ if striped && row % 2 == 1 => Some(theme.stripe),
                _ => None,
            };
            if let Some(bg) = row_bg {
                sink.rect_filled(row_rect, 0.0, bg);
            }
            // Modified cells.
            if state == RowState::Modified || state == RowState::Inserted {
                for info in &infos {
                    if !info.is_virtual && cells.is_modified(info.col) && state == RowState::Modified {
                        let r = Rect::from_min_max(pos2(info.x0, y0), pos2(info.x1, y0 + row_h));
                        sink.rect_filled(r, 0.0, theme.modified);
                    }
                }
            }
            // Selection segments of this row.
            for r in ranges {
                if row < r.r0 || row > r.r1 {
                    continue;
                }
                let c0 = r.c0.max(cols.start);
                let c1 = r.c1.min(cols.end.saturating_sub(1));
                if c0 > c1 || cols.is_empty() {
                    continue;
                }
                let x0 = geo.col_left(c0, sx);
                let x1 = geo.col_left(c1 + 1, sx);
                sink.rect_filled(Rect::from_min_max(pos2(x0, y0), pos2(x1, y0 + row_h)), 0.0, theme.selection);
            }
            // Cell contents.
            for info in &infos {
                let cell_rect = Rect::from_min_max(pos2(info.x0, y0), pos2(info.x1, y0 + row_h));
                let inner = cell_rect.shrink2(vec2(PAD, 0.0));
                if inner.width() <= 2.0 {
                    continue;
                }
                let virtual_value;
                let mut error_text: Option<String> = None;
                let cell: CellRef<'_> = if info.is_virtual {
                    match cells.virtual_value(info.col) {
                        Some(Ok(v)) => {
                            virtual_value = v;
                            CellRef::Value(&virtual_value)
                        }
                        Some(Err(e)) => {
                            error_text = Some(e.0);
                            CellRef::Missing
                        }
                        None => CellRef::Missing,
                    }
                } else {
                    cells.get(info.col)
                };
                let deleted = state == RowState::Deleted;
                let look: CellLook<'_> = match cell {
                    CellRef::Missing => match &error_text {
                        Some(_) => CellLook { text: "#ERR".into(), color: theme.error, align: Align::Min, badge: None, suffix: None, checkbox: None },
                        None => continue,
                    },
                    CellRef::Default => CellLook { text: default_text.into(), color: theme.weak, align: Align::Min, badge: None, suffix: None, checkbox: None },
                    // Evaluated by the database when saved: as typed, marked as SQL.
                    CellRef::Expr(e) => CellLook { text: e.into(), color: theme.text, align: Align::Min, badge: Some("SQL"), suffix: None, checkbox: None },
                    CellRef::Value(v) if v.is_null() => CellLook { text: null_text.into(), color: theme.weak, align: Align::Min, badge: None, suffix: None, checkbox: None },
                    CellRef::Value(v) => {
                        // Foreign key values look like others: their button (below) leads on.
                        let color = if deleted { theme.weak } else { theme.text };
                        match v {
                            Value::Bool(b) if checkbox_style => CellLook { text: "".into(), color, align: Align::Center, badge: None, suffix: None, checkbox: Some(*b) },
                            Value::Bytes(b) if detect_image(b).is_some() => {
                                let kind = detect_image(b).map_or("", |k| k.label());
                                CellLook { text: format!("{kind} {}", byte_size_text(b.len())).into(), color, align: Align::Min, badge: Some("IMG"), suffix: None, checkbox: None }
                            }
                            _ => {
                                let text = info.fmt.format(v);
                                let suffix = if info.dict { model.describe_value(info.col, v) } else { None };
                                CellLook {
                                    text,
                                    color,
                                    align: if is_number(v) && suffix.is_none() { Align::Max } else { Align::Min },
                                    badge: badge_for(v, info.kind),
                                    suffix,
                                    checkbox: None,
                                }
                            }
                        }
                    }
                };
                let mut text_rect = inner;
                // Badge.
                if let Some(b) = look.badge {
                    let g = sink.painter.layout_no_wrap(b.to_string(), badge_font.clone(), theme.badge_text);
                    let bw = g.rect.width() + 6.0;
                    if bw + 8.0 < inner.width() {
                        let br = Rect::from_center_size(pos2(inner.left() + bw / 2.0, inner.center().y), vec2(bw, (g.rect.height() + 2.0).min(row_h - 4.0)));
                        sink.rect_filled(br, CornerRadius::same(3), theme.badge_bg);
                        sink.galley(pos2(br.left() + 3.0, br.center().y - g.rect.height() / 2.0), g, theme.badge_text);
                        text_rect.min.x = br.right() + 4.0;
                    }
                }
                if let Some(checked) = look.checkbox {
                    let side = (row_h - 8.0).clamp(8.0, 14.0);
                    let r = Rect::from_center_size(inner.center(), vec2(side, side));
                    sink.rect_stroke(r, CornerRadius::same(2), Stroke::new(1.0, theme.weak), StrokeKind::Inside);
                    if checked {
                        let s = Stroke::new(1.6, theme.bool_true);
                        let a = pos2(r.left() + side * 0.22, r.center().y);
                        let m = pos2(r.left() + side * 0.42, r.bottom() - side * 0.25);
                        let z = pos2(r.right() - side * 0.18, r.top() + side * 0.22);
                        sink.line_segment([a, m], s);
                        sink.line_segment([m, z], s);
                    }
                    continue;
                }
                // A foreign key value has a button at its right that opens the row it references.
                let mut fk_hover: Option<Rect> = None;
                if info.fk
                    && matches!(cell, CellRef::Value(v) if !v.is_null())
                    && let Some(b) = fk_button_rect(inner)
                {
                    text_rect.max.x = b.left() - 2.0;
                    let hot = pointer.is_some_and(|p| b.contains(p));
                    if hot {
                        let bg = Rect::from_center_size(b.center(), vec2(b.width(), (row_h - 4.0).min(b.width() + 2.0)));
                        sink.rect_filled(bg, CornerRadius::same(3), mix(theme.bg, theme.text, 0.12));
                        fk_hover = Some(b);
                    }
                    let g = if hot { fk_hot.clone() } else { fk_idle.clone() };
                    let color = if hot { theme.link } else { theme.weak };
                    sink.galley(b.center() - g.rect.size() / 2.0, g, color);
                }
                let placed = place_text(glyphs, &mut sink, &mut batch, &font, &look.text, text_rect, look.align, look.color);
                let mut elided = placed.elided || look.text.contains('¶');
                if let Some(desc) = look.suffix {
                    let start = placed.left + placed.width + 6.0;
                    if start + 12.0 < text_rect.right() {
                        let r = Rect::from_min_max(pos2(start, text_rect.top()), text_rect.max);
                        let p2 = place_text(glyphs, &mut sink, &mut batch, &font, desc, r, Align::Min, theme.weak);
                        elided |= p2.elided;
                    } else {
                        elided = true;
                    }
                }
                if deleted && placed.width > 0.0 {
                    lines.push(Line { x0: placed.left, x1: placed.left + placed.width, y: inner.center().y, color: theme.weak });
                }
                let hovered = hover.is_some_and(|h| h.row == row && h.col == info.pos);
                if hovered && info.fk && command_down && placed.width > 0.0 {
                    lines.push(Line { x0: placed.left, x1: placed.left + placed.width, y: inner.center().y + font.size * 0.55, color: theme.link });
                }
                // Dates and times show every digit and their time zone on hover
                // (the cell shows milliseconds), as in DBeaver.
                let temporal = matches!(cell, CellRef::Value(Value::DateTime(_) | Value::DateTimeTz(_) | Value::Time(_)));
                if hovered {
                    if let Some(e) = &error_text {
                        tooltip = Some((cell_rect, e.clone()));
                    } else if elided || look.badge.is_some() || temporal {
                        let full = match cell {
                            CellRef::Value(v) => {
                                let mut t = match v {
                                    Value::Bytes(b) => trf("Binary value, {}", &[&byte_size_text(b.len())]),
                                    other => other.to_text().into_owned(),
                                };
                                if t.chars().count() > max_tip {
                                    t = t.chars().take(max_tip).collect::<String>() + "…";
                                }
                                if let Some(d) = look.suffix {
                                    t = format!("{t} — {d}");
                                }
                                if let Value::DateTimeTz(dt) = v {
                                    t = format!("{t}\n{}: UTC{}", tr("Time zone"), dt.format("%:z"));
                                }
                                t
                            }
                            _ => String::new(),
                        };
                        if !full.is_empty() {
                            tooltip = Some((cell_rect, full));
                        }
                    }
                }
                if let Some(b) = fk_hover
                    && let Some(i) = model.foreign_key_for_column(info.col)
                {
                    tooltip = Some((b, trf("Open the referenced row in {}", &[&model.foreign_keys()[i].ref_table.display_path()])));
                }
            }
        });
        if !fresh {
            // The fetch task holds the store lock (e.g. while spilling pages):
            // repeat the last body, shifted by the scroll change, and retry soon.
            if let Some(cache) = &self.state.rt.body_cache {
                let delta = vec2(cache.scroll.0 - sx, (cache.scroll.1 - sy) as f32) + (geo.body.min - cache.body.min);
                painter.extend(cache.shapes.iter().cloned().map(|mut s| {
                    s.translate(delta);
                    s
                }));
            }
            self.ctx.request_repaint_after(std::time::Duration::from_millis(8));
            self.hover_tooltip = None;
            return;
        }
        if let Some(text) = batch.into_shape(glyphs) {
            sink.shapes.push(text);
        }
        for l in &lines {
            sink.hline(l.x0..=l.x1, l.y, Stroke::new(1.0, l.color));
        }
        // Grid lines.
        let line = Stroke::new(1.0, theme.line);
        let last_row_bottom = geo.row_top(rows.end, sy).min(body.bottom());
        for pos in cols.clone() {
            let x = (geo.col_left(pos + 1, sx) - 0.5).round() + 0.5;
            sink.vline(x, body.top()..=last_row_bottom, line);
        }
        let faint = Stroke::new(1.0, mix(theme.line, theme.bg, 0.45));
        for row in rows.clone() {
            let y = (geo.row_top(row + 1, sy) - 0.5).round() + 0.5;
            sink.hline(body.left()..=content_right, y, faint);
        }
        // Active cell.
        if let Some(p) = (!self.state.rt.selection.is_empty()).then(|| self.state.rt.selection.active())
            && rows.contains(&p.row)
            && cols.contains(&p.col)
        {
            let r = geo.cell_rect(p.row, p.col, sx, sy);
            let stroke = if self.focused { theme.active_border } else { mix(theme.active_border, theme.bg, 0.5) };
            sink.rect_stroke(r, 0.0, Stroke::new(2.0, stroke), StrokeKind::Inside);
        }
        // Drop indicator while moving a column.
        if let Drag::Move { active: true, .. } = self.state.rt.drag
            && let Some(p) = self.pointer()
        {
            let to = self.drop_position(p.x);
            let x = geo.col_left(to, sx);
            sink.vline(x, body.y_range(), Stroke::new(2.0, theme.active_border));
        }
        painter.extend(sink.shapes.iter().cloned());
        self.state.rt.body_cache = Some(BodyCache { shapes: sink.shapes, scroll: (sx, sy), body: geo.body });
        self.hover_tooltip = tooltip;
    }

    fn paint_rownums(&mut self, ui: &Ui, geo: &Geo) {
        if geo.rownums.width() <= 0.0 {
            return;
        }
        let painter = ui.painter_at(geo.rownums);
        let theme = self.theme;
        painter.rect_filled(geo.rownums, 0.0, theme.header_bg);
        let sy = self.state.rt.scroll_y;
        let rows = geo.visible_rows(sy);
        let mut batch = TextBatch::new();
        let font = self.font.clone();
        let model = &*self.model;
        let selection = &self.state.rt.selection;
        let glyphs = &mut self.state.rt.glyphs;
        let mut buf = String::with_capacity(12);
        for row in rows.clone() {
            let y0 = geo.row_top(row, sy);
            let r = Rect::from_min_max(pos2(geo.rownums.left(), y0), pos2(geo.rownums.right(), y0 + geo.row_h));
            if selection.row_has_selection(row) {
                painter.rect_filled(r, 0.0, theme.header_selected_bg);
            }
            let state = model.row_state(row);
            let marker = match state {
                RowState::Inserted => Some(theme.bool_true),
                RowState::Deleted => Some(theme.error),
                RowState::Modified => Some(theme.warn),
                RowState::Unchanged => None,
            };
            if let Some(c) = marker {
                painter.rect_filled(Rect::from_min_size(r.min, vec2(3.0, geo.row_h)), 0.0, c);
            }
            buf.clear();
            use std::fmt::Write as _;
            let _ = write!(buf, "{}", row + 1);
            let inner = r.shrink2(vec2(PAD + 2.0, 0.0));
            let color = if state == RowState::Inserted { theme.bool_true } else { theme.rownum_text };
            if glyphs.place(&painter, &mut batch, &buf, inner, Align::Max, color).is_none() {
                place_slow(&painter, &font, &buf, inner, Align::Max, color);
            }
        }
        batch.paint(&painter, glyphs);
        let line = Stroke::new(1.0, theme.line);
        painter.vline(geo.rownums.right() - 0.5, geo.rownums.y_range(), line);
        let faint = Stroke::new(1.0, mix(theme.line, theme.bg, 0.45));
        for row in rows {
            let y = (geo.row_top(row + 1, sy) - 0.5).round() + 0.5;
            painter.hline(geo.rownums.x_range(), y, faint);
        }
    }

    fn paint_header(&mut self, ui: &Ui, geo: &Geo) {
        let painter = ui.painter_at(geo.header);
        let theme = self.theme;
        painter.rect_filled(geo.header, 0.0, theme.header_bg);
        let sx = self.state.rt.scroll_x;
        let cols = geo.visible_cols(sx);
        let hover = ui.input(|i| i.pointer.hover_pos()).filter(|p| geo.header.contains(*p));
        let key_cols: Vec<usize> = self.model.editability().as_ref().map(|t| t.key_columns.clone()).unwrap_or_default();
        let multi_sort = self.state.sort.len() > 1;
        for pos in cols.clone() {
            let col = geo.cols[pos];
            let x0 = geo.col_left(pos, sx);
            let x1 = geo.col_left(pos + 1, sx);
            let r = Rect::from_min_max(pos2(x0, geo.header.top()), pos2(x1, geo.header.bottom()));
            if self.state.rt.selection.is_column_selected(pos) {
                painter.rect_filled(r, 0.0, theme.header_selected_bg);
            } else if hover.is_some_and(|p| r.contains(p)) {
                painter.rect_filled(r, 0.0, mix(theme.header_bg, theme.text, 0.05));
            }
            let is_virtual = self.model.is_virtual_column(col);
            let mut left = r.left() + PAD;
            // Type icon (DBeaver's 123 / AZ), then key / FK icons.
            if let Some(ti) = kind_icon(self.model.column_kind(col))
                && r.width() > 64.0
            {
                let small = FontId::new(self.font.size * 0.85, self.font.family.clone());
                let ir = painter.text(pos2(left, r.center().y), Align2::LEFT_CENTER, ti, small, theme.weak);
                left = ir.right() + 4.0;
            }
            let icon = if key_cols.contains(&col) {
                Some((egui_phosphor::regular::KEY, theme.warn))
            } else if self.model.foreign_key_for_column(col).is_some() {
                Some((egui_phosphor::regular::LINK_SIMPLE, theme.link))
            } else {
                None
            };
            if let Some((icon, color)) = icon
                && r.width() > 48.0
            {
                let small = FontId::new(self.font.size * 0.9, self.font.family.clone());
                let ir = painter.text(pos2(left, r.center().y), Align2::LEFT_CENTER, icon, small, color);
                left = ir.right() + 3.0;
            }
            let name = self.model.column_name(col);
            let text_rect = Rect::from_min_max(pos2(left, r.top()), pos2(r.right() - SORT_ZONE - MENU_ZONE, r.bottom()));
            if text_rect.width() > 4.0 {
                let mut job =
                    LayoutJob::single_section(name.to_string(), TextFormat { font_id: self.font.clone(), color: theme.header_text, italics: is_virtual, ..Default::default() });
                job.wrap = TextWrapping { max_width: text_rect.width(), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
                let g = painter.layout_job(job);
                painter.galley(pos2(text_rect.left(), r.center().y - g.rect.height() / 2.0), g, theme.header_text);
            }
            // Column menu button (▾).
            let menu = Rect::from_min_max(pos2(r.right() - MENU_ZONE, r.top()), r.max);
            let header_hover = hover.is_some_and(|p| r.contains(p));
            let menu_open = self.state.rt.header_popup.is_some_and(|(c, ..)| c == col);
            let menu_color = if menu_open || hover.is_some_and(|p| menu.contains(p)) {
                theme.header_text
            } else if header_hover {
                theme.weak
            } else {
                theme.weak.gamma_multiply(0.55)
            };
            if r.width() > MENU_ZONE + 8.0 {
                if menu_open || hover.is_some_and(|p| menu.contains(p)) {
                    painter.rect_filled(menu.shrink(2.0), CornerRadius::same(3), mix(theme.header_bg, theme.text, 0.10));
                }
                let small = FontId::new(self.font.size * 0.8, self.font.family.clone());
                painter.text(menu.center(), Align2::CENTER_CENTER, egui_phosphor::regular::CARET_DOWN, small, menu_color);
            }
            // Sort indicator.
            let sort_idx = self.state.sort.iter().position(|k| k.column == col);
            let zone = Rect::from_min_max(pos2(r.right() - MENU_ZONE - SORT_ZONE, r.top()), pos2(r.right() - MENU_ZONE, r.bottom()));
            // The whole header cell sorts on click; hint the sort arrow while hovering it.
            let zone_hover = header_hover && !hover.is_some_and(|p| menu.contains(p));
            if let Some(i) = sort_idx {
                let k = self.state.sort[i];
                let icon = if k.descending { egui_phosphor::regular::ARROW_DOWN } else { egui_phosphor::regular::ARROW_UP };
                let small = FontId::new(self.font.size * 0.85, self.font.family.clone());
                painter.text(zone.center(), Align2::CENTER_CENTER, icon, small.clone(), theme.active_border);
                if multi_sort {
                    let tiny = FontId::new(self.font.size * 0.6, self.font.family.clone());
                    painter.text(pos2(zone.right() - 2.0, zone.bottom() - 2.0), Align2::RIGHT_BOTTOM, (i + 1).to_string(), tiny, theme.weak);
                }
            } else if zone_hover {
                let small = FontId::new(self.font.size * 0.85, self.font.family.clone());
                painter.text(zone.center(), Align2::CENTER_CENTER, egui_phosphor::regular::ARROW_UP, small, theme.weak);
            }
            painter.vline((x1 - 0.5).round() + 0.5, r.y_range(), Stroke::new(1.0, theme.line));
        }
        painter.hline(geo.header.x_range(), geo.header.bottom() - 0.5, Stroke::new(1.0, theme.line));
        // Ghost of a column being moved.
        if let (Drag::Move { from, active: true, .. }, Some(p)) = (self.state.rt.drag, self.pointer())
            && let Some(&col) = geo.cols.get(from)
        {
            let w = self.state.column_width(col);
            let ghost = Rect::from_center_size(pos2(p.x, geo.header.center().y), vec2(w, geo.header.height()));
            let fg = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, self.id.with("ghost")));
            fg.rect_filled(ghost, CornerRadius::same(3), theme.header_selected_bg.gamma_multiply(0.85));
            fg.text(ghost.center(), Align2::CENTER_CENTER, self.model.column_name(col), self.font.clone(), theme.header_text);
        }
    }

    fn paint_corner(&mut self, ui: &Ui, geo: &Geo) {
        if geo.corner.width() <= 0.0 || geo.corner.height() <= 0.0 {
            return;
        }
        let painter = ui.painter_at(geo.corner);
        let theme = self.theme;
        painter.rect_filled(geo.corner, 0.0, theme.header_bg);
        if self.state.rt.selection.is_all() {
            painter.rect_filled(geo.corner, 0.0, theme.header_selected_bg);
        }
        let (icon, color) = if self.model.is_editable() && self.options.allow_editing {
            (egui_phosphor::regular::PENCIL_SIMPLE, theme.weak)
        } else {
            (egui_phosphor::regular::LOCK_SIMPLE, theme.warn)
        };
        painter.text(geo.corner.center(), Align2::CENTER_CENTER, icon, self.font.clone(), color);
        painter.vline(geo.corner.right() - 0.5, geo.corner.y_range(), Stroke::new(1.0, theme.line));
        painter.hline(geo.corner.x_range(), geo.corner.bottom() - 0.5, Stroke::new(1.0, theme.line));
    }

    fn paint_scrollbars(&mut self, ui: &Ui, geo: &Geo) {
        let painter = ui.painter_at(geo.rect);
        // A thumb that stands out from the track in both themes, stronger when used.
        let track_c = mix(self.theme.bg, self.theme.text, 0.05);
        let (thumb_c, active_c) = (mix(self.theme.bg, self.theme.text, 0.32), mix(self.theme.bg, self.theme.text, 0.55));
        let hover_pos = ui.input(|i| i.pointer.hover_pos());
        if let Some(bar) = geo.vbar {
            painter.rect_filled(bar, 0.0, track_c);
            let (thumb, _) = self.v_thumb(bar);
            let active = matches!(self.state.rt.drag, Drag::VThumb { .. }) || hover_pos.is_some_and(|p| bar.contains(p));
            let c = if active { active_c } else { thumb_c };
            painter.rect_filled(thumb.shrink2(vec2(1.0, 0.0)), CornerRadius::same(4), c);
        }
        if let Some(bar) = geo.hbar {
            painter.rect_filled(bar, 0.0, track_c);
            let (thumb, _) = self.h_thumb(bar);
            let active = matches!(self.state.rt.drag, Drag::HThumb { .. }) || hover_pos.is_some_and(|p| bar.contains(p));
            let c = if active { active_c } else { thumb_c };
            painter.rect_filled(thumb.shrink2(vec2(0.0, 1.0)), CornerRadius::same(4), c);
        }
        if let (Some(v), Some(h)) = (geo.vbar, geo.hbar) {
            painter.rect_filled(Rect::from_min_max(pos2(v.left(), h.top()), pos2(v.right(), h.bottom())), 0.0, track_c);
        }
    }

    fn paint_overlays(&mut self, ui: &Ui, geo: &Geo) {
        let painter = ui.painter_at(geo.body);
        let theme = self.theme;
        let small = FontId::new(self.font.size * 0.92, self.font.family.clone());
        let mut y = geo.body.top() + 6.0;
        let mut badge = |text: String, color: Color32| {
            let g = painter.layout_no_wrap(text, small.clone(), color);
            let r = Rect::from_min_size(pos2(geo.body.right() - g.rect.width() - 16.0, y), g.rect.size() + vec2(10.0, 6.0));
            painter.rect_filled(r, CornerRadius::same(4), mix(theme.header_bg, theme.bg, 0.2).gamma_multiply(0.95));
            painter.rect_stroke(r, CornerRadius::same(4), Stroke::new(1.0, theme.line), StrokeKind::Inside);
            painter.galley(r.min + vec2(5.0, 3.0), g, color);
            y = r.bottom() + 4.0;
        };
        if self.model.is_view_job_running() {
            badge(format!("{} {}", egui_phosphor::regular::HOURGLASS, tr("Sorting / filtering…")), theme.text);
        } else if self.model.is_view_stale() {
            let n = self.model.rows_outside_view();
            badge(trf("{} newly fetched rows are not in the client sort/filter yet", &[&n]), theme.warn);
        }
        let now = ui.input(|i| i.time);
        let mut clear = false;
        if let Some((msg, error, at)) = &mut self.state.rt.message {
            if *at == 0.0 {
                *at = now;
            }
            let age = now - *at;
            const SHOW: f64 = 4.0;
            if age < SHOW {
                badge(msg.clone(), if *error { theme.error } else { theme.text });
                ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(SHOW - age + 0.05));
            } else {
                clear = true;
            }
        }
        if clear {
            self.state.rt.message = None;
        }
    }

    /// While the first rows are fetched: a spinner with "Fetching…", and
    /// Cancel once it takes longer than [`CANCEL_AFTER`].
    pub fn fetching_overlay(&mut self, ui: &mut Ui) {
        let Some(running) = self.model.fetch.fetching_for() else { return };
        if self.geo.nrows > 0 || self.geo.cols.is_empty() {
            return;
        }
        let center = self.geo.body.center();
        // The spinner and "Fetching…" as one line, centred; Cancel centred under it.
        let text = egui::WidgetText::from(egui::RichText::new(tr("Fetching…"))).into_galley(ui, None, f32::INFINITY, egui::TextStyle::Body);
        let (spin, gap) = (14.0, 8.0);
        let left = center.x - (spin + gap + text.size().x) / 2.0;
        ui.put(Rect::from_center_size(pos2(left + spin / 2.0, center.y), vec2(spin, spin)), egui::Spinner::new().size(spin).color(self.theme.weak));
        let text_rect = Rect::from_min_size(pos2(left + spin + gap, center.y - text.size().y / 2.0), text.size());
        ui.put(text_rect, egui::Label::new(egui::RichText::new(tr("Fetching…")).color(self.theme.weak)).selectable(false));
        if running >= CANCEL_AFTER {
            let label = egui::WidgetText::from(tr("Cancel")).into_galley(ui, None, f32::INFINITY, egui::TextStyle::Button);
            let size = vec2(label.size().x + 2.0 * ui.spacing().button_padding.x + 16.0, ui.spacing().interact_size.y + 2.0);
            let at = Rect::from_center_size(pos2(center.x, center.y + 30.0), size);
            if ui.put(at, egui::Button::new(tr("Cancel"))).on_hover_text(tr("Stop the query")).clicked() {
                self.actions.push(GridAction::CancelFetch);
            }
        } else {
            ui.ctx().request_repaint_after(CANCEL_AFTER - running);
        }
    }

    /// Tooltip for elided cells.
    pub fn cell_tooltip(&mut self, ui: &mut Ui) {
        let Some((rect, text)) = self.hover_tooltip.take() else { return };
        if self.state.rt.editing.is_some() || !matches!(self.state.rt.drag, Drag::None) {
            return;
        }
        let Some(c) = self.hover_cell else { return };
        let resp = ui.interact(rect, self.id.with(("cell_tip", c.row, c.col)), egui::Sense::hover());
        resp.on_hover_ui(|ui| {
            ui.set_max_width(ui.spacing().tooltip_width * 1.6);
            ui.add(egui::Label::new(egui::RichText::new(text).monospace()).wrap());
        });
    }
}

/// Small icon of a column's kind in its header; `None` for other types.
pub(crate) fn kind_icon(kind: DataKind) -> Option<&'static str> {
    use egui_phosphor::regular as i;
    Some(match kind {
        DataKind::Bool => i::TOGGLE_LEFT,
        DataKind::Int | DataKind::Float | DataKind::Decimal => i::HASH,
        DataKind::Text => i::TEXT_T,
        DataKind::Enum => i::TAG,
        DataKind::Binary | DataKind::Bit => i::BINARY,
        DataKind::Date => i::CALENDAR_BLANK,
        DataKind::Time | DataKind::TimeTz => i::CLOCK,
        DataKind::DateTime | DataKind::DateTimeTz => i::CALENDAR_DOTS,
        DataKind::Interval => i::HOURGLASS,
        DataKind::Uuid => i::FINGERPRINT,
        DataKind::Json => i::BRACKETS_CURLY,
        DataKind::Xml => i::CODE,
        DataKind::Array => i::BRACKETS_SQUARE,
        DataKind::Geometry => i::MAP_PIN,
        DataKind::Network => i::NETWORK,
        DataKind::Range => i::ARROWS_LEFT_RIGHT,
        DataKind::Other => return None,
    })
}

/// Place text with the fast path, falling back to egui layout.
#[allow(clippy::too_many_arguments)]
fn place_text(glyphs: &mut GlyphCache, sink: &mut Sink<'_>, batch: &mut TextBatch, font: &FontId, text: &str, rect: Rect, align: Align, color: Color32) -> Placed {
    match glyphs.place(sink.painter, batch, text, rect, align, color) {
        Some(p) => p,
        None => {
            let (shape, placed) = layout_slow(sink.painter, font, text, rect, align, color);
            sink.shapes.push(shape);
            placed
        }
    }
}
