//! Query builder UI: canvas with table boxes (tick columns, drag a column
//! onto another table's column to join, right click a join to change its
//! type), grids for columns / conditions / grouping / ordering and a live
//! SQL preview.

use super::model::{
    Aggregate, Connector, JoinKind, OPERATORS, QB_FONT, QB_HEADER, QB_ROW, QbCondition, QbJoin,
    QbSelectItem, QueryModel,
};
use crate::common::colors;
use crate::common::paint::{self, font_px};
use crate::common::view::{ViewTransform, distance_to_polyline, handle_pan_zoom, round_pos};
use crate::er::scene::route;
use dbiji_core::dialect::GenericDialect;
use dbiji_core::{Dialect, tr, trf};
use egui::{
    Align2, CornerRadius, CursorIcon, Id, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui,
    Vec2, pos2, vec2,
};
use egui_phosphor::regular as icon;
use std::fmt;
use std::sync::Arc;

/// Bottom tabs of the builder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum QbTab {
    /// Select list.
    #[default]
    Columns,
    /// WHERE conditions.
    Conditions,
    /// GROUP BY and HAVING.
    Grouping,
    /// ORDER BY.
    Order,
}

impl QbTab {
    const ALL: [QbTab; 4] = [
        QbTab::Columns,
        QbTab::Conditions,
        QbTab::Grouping,
        QbTab::Order,
    ];
    fn label(self) -> &'static str {
        match self {
            QbTab::Columns => "Columns",
            QbTab::Conditions => "Conditions",
            QbTab::Grouping => "Grouping",
            QbTab::Order => "Order",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum QbDrag {
    Pan,
    MoveTable(String),
    Join {
        alias: String,
        column: String,
        current: Pos2,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum QbHit {
    Header(String),
    Close(String),
    Row(String, String),
    Join(usize),
}

#[derive(Debug, Clone, PartialEq)]
enum QbContext {
    Join(usize),
    Table(String),
    Background,
}

/// View state of the query builder.
pub struct QbViewState {
    /// Dialect used for quoting and LIMIT syntax in the generated SQL.
    pub dialect: Arc<dyn Dialect>,
    /// Canvas zoom.
    pub zoom: f32,
    /// Canvas pan offset.
    pub pan: Vec2,
    /// Active bottom tab.
    pub tab: QbTab,
    /// Show the toolbar.
    pub show_toolbar: bool,
    drag: Option<QbDrag>,
    context: Option<QbContext>,
    alias_edit: String,
    last_canvas: Rect,
}

impl fmt::Debug for QbViewState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QbViewState")
            .field("dialect", &self.dialect.kind())
            .field("zoom", &self.zoom)
            .field("pan", &self.pan)
            .field("tab", &self.tab)
            .finish_non_exhaustive()
    }
}

impl Default for QbViewState {
    fn default() -> Self {
        Self::new(Arc::new(GenericDialect::new()))
    }
}

impl QbViewState {
    /// State generating SQL for `dialect`.
    pub fn new(dialect: Arc<dyn Dialect>) -> Self {
        QbViewState {
            dialect,
            zoom: 1.0,
            pan: vec2(10.0, 10.0),
            tab: QbTab::Columns,
            show_toolbar: true,
            drag: None,
            context: None,
            alias_edit: String::new(),
            last_canvas: Rect::NOTHING,
        }
    }
}

/// Something the application has to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QbEvent {
    /// The user wants to add a table: show a picker and call
    /// [`QueryModel::add_table`].
    AddTableRequested,
    /// Run the generated SQL.
    RunQuery(String),
    /// Open the generated SQL in a SQL editor.
    OpenInEditor(String),
    /// The model changed (save / mark dirty).
    Changed,
}

/// Result of [`show_query_builder`].
#[derive(Debug, Clone, Default)]
pub struct QbResponse {
    /// Events produced this frame.
    pub events: Vec<QbEvent>,
    /// SQL of the current model.
    pub sql: String,
}

/// Shows the query builder filling the available space.
pub fn show_query_builder(
    ui: &mut Ui,
    model: &mut QueryModel,
    state: &mut QbViewState,
) -> QbResponse {
    let id = ui.id().with("dbiji_qb");
    let before = model.clone();
    let mut events = Vec::new();
    let sql_now = model.to_sql(state.dialect.as_ref());
    if state.show_toolbar {
        toolbar(ui, model, &sql_now, &mut events);
    }
    egui::Panel::bottom(id.with("bottom"))
        .resizable(true)
        .default_size(260.0)
        .show(ui, |ui| {
            egui::Panel::right(id.with("sql"))
                .resizable(true)
                .default_size(380.0)
                .show(ui, |ui| {
                    sql_preview(ui, &sql_now, &mut events);
                });
            tabs(ui, model, state);
        });
    canvas(ui, model, state, id, &mut events);
    if *model != before {
        events.push(QbEvent::Changed);
    }
    QbResponse {
        events,
        sql: model.to_sql(state.dialect.as_ref()),
    }
}

fn toolbar(ui: &mut Ui, model: &mut QueryModel, sql: &str, events: &mut Vec<QbEvent>) {
    ui.horizontal(|ui| {
        if ui
            .button((dbiji_icons::ic(icon::PLUS), tr("Add table")))
            .clicked()
        {
            events.push(QbEvent::AddTableRequested);
        }
        let has_sql = !sql.is_empty();
        if ui
            .add_enabled(
                has_sql,
                egui::Button::new((dbiji_icons::ic(icon::PLAY), tr("Run"))),
            )
            .clicked()
        {
            events.push(QbEvent::RunQuery(sql.to_string()));
        }
        if ui
            .add_enabled(
                has_sql,
                egui::Button::new(format!(
                    "{} {}",
                    icon::ARROW_SQUARE_OUT,
                    tr("Open in editor")
                )),
            )
            .clicked()
        {
            events.push(QbEvent::OpenInEditor(sql.to_string()));
        }
        ui.separator();
        ui.checkbox(&mut model.distinct, "DISTINCT");
        let mut limited = model.limit.is_some();
        if ui.checkbox(&mut limited, tr("Limit")).changed() {
            model.limit = if limited { Some(100) } else { None };
        }
        if let Some(l) = model.limit.as_mut() {
            ui.add(egui::DragValue::new(l).range(1..=u64::MAX).speed(1.0));
        }
    });
}

fn sql_preview(ui: &mut Ui, sql: &str, events: &mut Vec<QbEvent>) {
    ui.horizontal(|ui| {
        ui.strong(tr("SQL"));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button((dbiji_icons::ic(icon::COPY), tr("Copy")))
                .clicked()
            {
                ui.ctx().copy_text(sql.to_string());
            }
            if ui
                .small_button(dbiji_icons::ic(icon::ARROW_SQUARE_OUT))
                .on_hover_text(tr("Open in editor"))
                .clicked()
                && !sql.is_empty()
            {
                events.push(QbEvent::OpenInEditor(sql.to_string()));
            }
        });
    });
    egui::ScrollArea::both()
        .id_salt("dbiji_qb_sql")
        .auto_shrink(false)
        .show(ui, |ui| {
            let mut text = sql;
            ui.add(
                egui::TextEdit::multiline(&mut text)
                    .code_editor()
                    .desired_width(f32::INFINITY)
                    .hint_text(tr("Add tables to build a query")),
            );
        });
}

// ---------------------------------------------------------------- canvas

fn join_points(model: &QueryModel, j: &QbJoin) -> Option<Vec<Pos2>> {
    let l = model.table(&j.left_alias)?;
    let r = model.table(&j.right_alias)?;
    let y1 = l.column_y(&j.left_column)?;
    let y2 = r.column_y(&j.right_column)?;
    Some(route(l.rect(), y1, r.rect(), y2, 0.0).points)
}

fn join_label_pos(points: &[Pos2]) -> Option<Pos2> {
    match points.len() {
        0 => None,
        1 => points.first().copied(),
        2 | 3 => Some(points[0].lerp(points[points.len() - 1], 0.5)),
        _ => Some(points[1].lerp(points[2], 0.5)),
    }
}

fn kind_symbol(k: JoinKind) -> &'static str {
    match k {
        JoinKind::Inner => "INNER",
        JoinKind::Left => "LEFT",
        JoinKind::Right => "RIGHT",
        JoinKind::Full => "FULL",
    }
}

fn hit_test(model: &QueryModel, world: Pos2, zoom: f32) -> Option<QbHit> {
    for t in model.tables.iter().rev() {
        let r = t.rect();
        if !r.contains(world) {
            continue;
        }
        if world.y < r.top() + QB_HEADER {
            if world.x > r.right() - 22.0 {
                return Some(QbHit::Close(t.alias.clone()));
            }
            return Some(QbHit::Header(t.alias.clone()));
        }
        let i = ((world.y - r.top() - QB_HEADER) / QB_ROW).floor();
        if i >= 0.0
            && let Some(c) = t.columns.get(i as usize)
        {
            return Some(QbHit::Row(t.alias.clone(), c.name.clone()));
        }
        return Some(QbHit::Header(t.alias.clone()));
    }
    let tol = 6.0 / zoom.max(0.1);
    for (i, j) in model.joins.iter().enumerate() {
        if let Some(pts) = join_points(model, j) {
            let near_label = join_label_pos(&pts).is_some_and(|p| (p - world).length() < 18.0);
            if near_label || distance_to_polyline(world, &pts) <= tol {
                return Some(QbHit::Join(i));
            }
        }
    }
    None
}

fn canvas(
    ui: &mut Ui,
    model: &mut QueryModel,
    state: &mut QbViewState,
    id: Id,
    events: &mut Vec<QbEvent>,
) {
    let rect = ui.available_rect_before_wrap();
    let rect = Rect::from_min_size(rect.min, rect.size().max(vec2(64.0, 64.0)));
    let response = ui.interact(rect, id.with("canvas"), Sense::click_and_drag());
    ui.advance_cursor_after_rect(rect);
    state.last_canvas = rect;
    let origin = rect.min;
    let mut view = ViewTransform::new(state.zoom, state.pan);
    if handle_pan_zoom(ui, &response, &mut view, origin, (0.4, 2.0), false).changed {
        state.zoom = view.zoom;
        state.pan = view.pan;
    }
    let view = ViewTransform::new(state.zoom, state.pan);
    let pointer = ui
        .input(|i| i.pointer.latest_pos())
        .filter(|p| rect.contains(*p));
    let hover = pointer.and_then(|p| hit_test(model, view.to_world(origin, p), view.zoom));

    if response.drag_started_by(egui::PointerButton::Primary) {
        let press = ui.input(|i| i.pointer.press_origin()).unwrap_or(origin);
        let world = view.to_world(origin, press);
        state.drag = Some(match hit_test(model, world, view.zoom) {
            Some(QbHit::Header(a)) | Some(QbHit::Close(a)) => QbDrag::MoveTable(a),
            Some(QbHit::Row(a, c)) => QbDrag::Join {
                alias: a,
                column: c,
                current: world,
            },
            _ => QbDrag::Pan,
        });
    }
    if response.dragged_by(egui::PointerButton::Primary) {
        let d = response.drag_delta();
        match &mut state.drag {
            Some(QbDrag::Pan) => state.pan += d,
            Some(QbDrag::MoveTable(a)) => {
                if let Some(t) = model.table_mut(a) {
                    t.pos += d / state.zoom.max(0.1);
                }
            }
            Some(QbDrag::Join { current, .. }) => {
                if let Some(p) = pointer {
                    *current = view.to_world(origin, p);
                }
            }
            None => {}
        }
    }
    if response.drag_stopped() {
        match state.drag.take() {
            Some(QbDrag::MoveTable(a)) => {
                if let Some(t) = model.table_mut(&a) {
                    t.pos = round_pos(t.pos);
                }
            }
            Some(QbDrag::Join {
                alias,
                column,
                current,
            }) => {
                if let Some(QbHit::Row(a2, c2)) = hit_test(model, current, view.zoom)
                    && a2 != alias
                {
                    model.add_join(QbJoin {
                        left_alias: alias,
                        left_column: column,
                        right_alias: a2,
                        right_column: c2,
                        kind: JoinKind::Inner,
                    });
                }
            }
            _ => {}
        }
    }
    // MySQL and MariaDB have no FULL JOIN: it is not offered there.
    let full_join = !matches!(
        state.dialect.kind(),
        dbiji_core::DbKind::MySql | dbiji_core::DbKind::MariaDb
    );
    if response.clicked() {
        match hover.clone() {
            Some(QbHit::Row(a, c)) => {
                let selected = model
                    .table(&a)
                    .and_then(|t| t.columns.iter().find(|x| x.name == c))
                    .is_some_and(|x| x.selected);
                model.set_column_selected(&a, &c, !selected);
            }
            Some(QbHit::Close(a)) => model.remove_table(&a),
            Some(QbHit::Join(i)) => {
                // quick toggle; the full menu is on right click
                if let Some(j) = model.joins.get_mut(i) {
                    j.kind = match j.kind {
                        JoinKind::Inner => JoinKind::Left,
                        JoinKind::Left => JoinKind::Right,
                        JoinKind::Right if full_join => JoinKind::Full,
                        JoinKind::Right | JoinKind::Full => JoinKind::Inner,
                    };
                }
            }
            _ => {}
        }
    }
    if response.secondary_clicked() {
        state.context = Some(match hover.clone() {
            Some(QbHit::Join(i)) => QbContext::Join(i),
            Some(QbHit::Header(a)) | Some(QbHit::Close(a)) | Some(QbHit::Row(a, _)) => {
                state.alias_edit = a.clone();
                QbContext::Table(a)
            }
            None => QbContext::Background,
        });
    }
    if let Some(ctx_target) = state.context.clone() {
        let mut alias_edit = std::mem::take(&mut state.alias_edit);
        let menu = response.context_menu(|ui| match &ctx_target {
            QbContext::Join(i) => {
                if let Some(j) = model.joins.get_mut(*i) {
                    ui.label(
                        RichText::new(format!(
                            "{}.{} = {}.{}",
                            j.left_alias, j.left_column, j.right_alias, j.right_column
                        ))
                        .strong(),
                    );
                    ui.separator();
                    for k in JoinKind::ALL {
                        if k == JoinKind::Full && !full_join && j.kind != k {
                            continue;
                        }
                        if ui.radio(j.kind == k, tr(k.label())).clicked() {
                            j.kind = k;
                            ui.close();
                        }
                    }
                    ui.separator();
                    if ui
                        .button((dbiji_icons::ic(icon::TRASH), tr("Remove join")))
                        .clicked()
                    {
                        model.joins.remove(*i);
                        ui.close();
                    }
                }
            }
            QbContext::Table(a) => {
                ui.label(RichText::new(a.as_str()).strong());
                ui.separator();
                if ui
                    .button(format!(
                        "{} {}",
                        icon::CHECK_SQUARE,
                        tr("Select all columns")
                    ))
                    .clicked()
                {
                    let cols: Vec<String> = model
                        .table(a)
                        .map(|t| t.columns.iter().map(|c| c.name.clone()).collect())
                        .unwrap_or_default();
                    for c in cols {
                        model.set_column_selected(a, &c, true);
                    }
                    ui.close();
                }
                if ui
                    .button((dbiji_icons::ic(icon::SQUARE), tr("Clear selection")))
                    .clicked()
                {
                    let cols: Vec<String> = model
                        .table(a)
                        .map(|t| t.columns.iter().map(|c| c.name.clone()).collect())
                        .unwrap_or_default();
                    for c in cols {
                        model.set_column_selected(a, &c, false);
                    }
                    ui.close();
                }
                ui.horizontal(|ui| {
                    ui.label(tr("Alias"));
                    let r = ui.add(egui::TextEdit::singleline(&mut alias_edit).desired_width(90.0));
                    let apply = ui
                        .button(dbiji_icons::ic(icon::CHECK))
                        .on_hover_text(tr("Rename alias"))
                        .clicked()
                        || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                    if apply && model.rename_alias(a, &alias_edit) {
                        ui.close();
                    }
                });
                if ui
                    .button((dbiji_icons::ic(icon::TRASH), tr("Remove table")))
                    .clicked()
                {
                    model.remove_table(a);
                    ui.close();
                }
            }
            QbContext::Background => {
                if ui
                    .button((dbiji_icons::ic(icon::PLUS), tr("Add table…")))
                    .clicked()
                {
                    events.push(QbEvent::AddTableRequested);
                    ui.close();
                }
                if ui
                    .button((dbiji_icons::ic(icon::ARROWS_IN), tr("Reset view")))
                    .clicked()
                {
                    state.zoom = 1.0;
                    state.pan = vec2(10.0, 10.0);
                    ui.close();
                }
            }
        });
        state.alias_edit = alias_edit;
        // The menu closed: the join is no longer highlighted (and an index may now name another one).
        if menu.is_none() {
            state.context = None;
        }
    }

    // painting
    let painter = ui.painter_at(rect);
    let v = ui.visuals();
    painter.rect_filled(rect, 0.0, v.extreme_bg_color);
    let zoom = state.zoom;
    let line = v.text_color().gamma_multiply(0.6);
    for (i, j) in model.joins.iter().enumerate() {
        let Some(pts) = join_points(model, j) else {
            continue;
        };
        let hot = hover == Some(QbHit::Join(i)) || state.context == Some(QbContext::Join(i));
        let stroke = Stroke::new(
            if hot { 2.5 } else { 1.5 },
            if hot { v.selection.stroke.color } else { line },
        );
        let screen: Vec<Pos2> = pts.iter().map(|p| view.to_screen(origin, *p)).collect();
        if let (Some(a), Some(b)) = (screen.first(), screen.last()) {
            painter.circle_filled(*a, 3.0, stroke.color);
            painter.circle_filled(*b, 3.0, stroke.color);
        }
        painter.line(screen.clone(), stroke);
        if let (Some(p), Some(px)) = (join_label_pos(&pts), font_px(10.5, zoom)) {
            let galley =
                paint::single_line(&painter, kind_symbol(j.kind), px, false, f32::INFINITY);
            let sp = view.to_screen(origin, p);
            let r = Rect::from_center_size(sp, galley.size() + vec2(10.0, 4.0));
            painter.rect_filled(r, r.height() * 0.5, v.window_fill);
            painter.rect_stroke(r, r.height() * 0.5, stroke, StrokeKind::Inside);
            painter.galley(r.min + vec2(5.0, 2.0), galley, v.text_color());
        }
    }
    for t in &model.tables {
        let srect = view.rect_to_screen(origin, t.rect());
        if !srect.intersects(rect) {
            continue;
        }
        let radius = (5.0 * zoom).clamp(0.0, 10.0);
        painter.rect_filled(srect, radius, v.window_fill);
        let header = Rect::from_min_size(srect.min, vec2(srect.width(), QB_HEADER * zoom));
        let r8 = radius.round() as u8;
        painter.rect_filled(
            header,
            CornerRadius {
                nw: r8,
                ne: r8,
                sw: 0,
                se: 0,
            },
            colors::tint(v, colors::categorical(0, v.dark_mode), 0.14),
        );
        let hovered_header =
            matches!(&hover, Some(QbHit::Header(a)) | Some(QbHit::Close(a)) if *a == t.alias);
        if let Some(px) = font_px(QB_FONT + 0.5, zoom) {
            let x = header.left() + 8.0 * zoom;
            let r1 = paint::text(
                &painter,
                pos2(x, header.center().y),
                Align2::LEFT_CENTER,
                icon::TABLE,
                px,
                v.weak_text_color(),
                false,
                f32::INFINITY,
            );
            let r2 = paint::text(
                &painter,
                pos2(r1.right() + 5.0 * zoom, header.center().y),
                Align2::LEFT_CENTER,
                &t.obj.name,
                px,
                v.strong_text_color(),
                false,
                (header.width() * 0.6).max(20.0),
            );
            paint::text(
                &painter,
                pos2(r2.right() + 6.0 * zoom, header.center().y),
                Align2::LEFT_CENTER,
                &t.alias,
                px,
                v.weak_text_color(),
                false,
                f32::INFINITY,
            );
            let close_color = if matches!(&hover, Some(QbHit::Close(a)) if *a == t.alias) {
                v.error_fg_color
            } else {
                v.weak_text_color()
            };
            paint::text(
                &painter,
                pos2(header.right() - 8.0 * zoom, header.center().y),
                Align2::RIGHT_CENTER,
                icon::X,
                px,
                close_color,
                false,
                f32::INFINITY,
            );
        }
        for (i, c) in t.columns.iter().enumerate() {
            let top = srect.top() + (QB_HEADER + i as f32 * QB_ROW) * zoom;
            let row =
                Rect::from_min_size(pos2(srect.left(), top), vec2(srect.width(), QB_ROW * zoom));
            let hovered_row =
                matches!(&hover, Some(QbHit::Row(a, col)) if *a == t.alias && *col == c.name);
            let join_source = matches!(&state.drag, Some(QbDrag::Join { alias, column, .. }) if *alias == t.alias && *column == c.name);
            if hovered_row || join_source {
                painter.rect_filled(
                    row.shrink2(vec2(2.0, 0.0)),
                    2.0,
                    v.selection.bg_fill.gamma_multiply(0.35),
                );
            }
            let Some(px) = font_px(QB_FONT, zoom) else {
                continue;
            };
            let cb = Rect::from_center_size(
                pos2(row.left() + 14.0 * zoom, row.center().y),
                Vec2::splat(11.0 * zoom),
            );
            painter.rect_stroke(
                cb,
                2.0,
                Stroke::new(1.0, v.widgets.inactive.fg_stroke.color),
                StrokeKind::Inside,
            );
            if c.selected {
                painter.rect_filled(cb.shrink(1.0), 2.0, v.selection.bg_fill);
                painter.line(
                    vec![
                        cb.left_center() + vec2(2.0, 0.0),
                        cb.center_bottom() - vec2(0.5, 2.5),
                        cb.right_top() + vec2(-2.0, 2.5),
                    ],
                    Stroke::new(1.5, v.selection.stroke.color),
                );
            }
            let name_x = row.left() + 26.0 * zoom;
            let tr_rect = paint::text(
                &painter,
                pos2(row.right() - 8.0 * zoom, row.center().y),
                Align2::RIGHT_CENTER,
                &c.data_type,
                px - 1.0,
                v.weak_text_color(),
                false,
                row.width() * 0.45,
            );
            paint::text(
                &painter,
                pos2(name_x, row.center().y),
                Align2::LEFT_CENTER,
                &c.name,
                px,
                v.text_color(),
                false,
                (tr_rect.left() - name_x - 6.0).max(10.0),
            );
        }
        let stroke = if hovered_header {
            Stroke::new(1.5, v.selection.stroke.color)
        } else {
            v.widgets.noninteractive.bg_stroke
        };
        painter.rect_stroke(srect, radius, stroke, StrokeKind::Inside);
    }
    if let Some(QbDrag::Join {
        alias,
        column,
        current,
    }) = &state.drag
        && let Some(t) = model.table(alias)
        && let Some(y) = t.column_y(column)
    {
        let r = t.rect();
        let start = if current.x > r.center().x {
            pos2(r.right(), y)
        } else {
            pos2(r.left(), y)
        };
        paint::dashed_polyline(
            &painter,
            &[
                view.to_screen(origin, start),
                view.to_screen(origin, *current),
            ],
            Stroke::new(2.0, v.selection.stroke.color),
            6.0,
            4.0,
        );
    }
    if model.tables.is_empty() {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            tr("Add tables with the + button, then tick columns and drag columns onto each other to join"),
            egui::FontId::proportional(13.0),
            v.weak_text_color(),
        );
    }
    match &hover {
        Some(QbHit::Row(..)) | Some(QbHit::Close(..)) | Some(QbHit::Join(..)) => {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand)
        }
        Some(QbHit::Header(..)) => ui.ctx().set_cursor_icon(CursorIcon::Grab),
        None => {}
    }
    if let Some(QbHit::Row(..)) = &hover {
        response.on_hover_text_at_pointer(tr(
            "Click to select the column, drag onto a column of another table to join",
        ));
    }
}

// ---------------------------------------------------------------- grids

#[derive(Clone, Copy)]
enum RowAction {
    Up(usize),
    Down(usize),
    Delete(usize),
}

fn apply_row_action<T>(list: &mut Vec<T>, action: Option<RowAction>) {
    match action {
        Some(RowAction::Up(i)) if i > 0 && i < list.len() => list.swap(i, i - 1),
        Some(RowAction::Down(i)) if i + 1 < list.len() => list.swap(i, i + 1),
        Some(RowAction::Delete(i)) if i < list.len() => {
            list.remove(i);
        }
        _ => {}
    }
}

fn row_buttons(ui: &mut Ui, i: usize, action: &mut Option<RowAction>) {
    if ui
        .small_button(dbiji_icons::ic(icon::ARROW_UP))
        .on_hover_text(tr("Move up"))
        .clicked()
    {
        *action = Some(RowAction::Up(i));
    }
    if ui
        .small_button(dbiji_icons::ic(icon::ARROW_DOWN))
        .on_hover_text(tr("Move down"))
        .clicked()
    {
        *action = Some(RowAction::Down(i));
    }
    if ui
        .small_button(dbiji_icons::ic(icon::TRASH))
        .on_hover_text(tr("Remove"))
        .clicked()
    {
        *action = Some(RowAction::Delete(i));
    }
}

/// Text field with a menu of column references next to it.
fn expr_field(ui: &mut Ui, value: &mut String, refs: &[String], width: f32) {
    ui.add(
        egui::TextEdit::singleline(value)
            .desired_width(width)
            .hint_text(tr("column or expression")),
    );
    ui.menu_button(icon::LIST, |ui| {
        egui::ScrollArea::vertical()
            .max_height(260.0)
            .show(ui, |ui| {
                for r in refs {
                    if ui.button(r.as_str()).clicked() {
                        *value = r.clone();
                        ui.close();
                    }
                }
            });
    })
    .response
    .on_hover_text(tr("Pick a column"));
}

fn tabs(ui: &mut Ui, model: &mut QueryModel, state: &mut QbViewState) {
    ui.horizontal(|ui| {
        for t in QbTab::ALL {
            let count = match t {
                QbTab::Columns => model.select.len(),
                QbTab::Conditions => model.conditions.len(),
                QbTab::Grouping => model.group_by.len() + model.having.len(),
                QbTab::Order => model.order_by.len(),
            };
            let label = if count > 0 {
                format!("{} ({count})", tr(t.label()))
            } else {
                tr(t.label()).to_string()
            };
            ui.selectable_value(&mut state.tab, t, label);
        }
    });
    ui.separator();
    let refs = model.column_refs();
    egui::ScrollArea::vertical()
        .id_salt("dbiji_qb_tabs")
        .auto_shrink(false)
        .show(ui, |ui| match state.tab {
            QbTab::Columns => columns_grid(ui, model, &refs),
            QbTab::Conditions => {
                conditions_grid(ui, "dbiji_qb_where", &mut model.conditions, &refs);
            }
            QbTab::Grouping => grouping(ui, model, &refs),
            QbTab::Order => order_grid(ui, model, &refs),
        });
}

fn columns_grid(ui: &mut Ui, model: &mut QueryModel, refs: &[String]) {
    let mut action = None;
    let mut changed = false;
    egui::Grid::new("dbiji_qb_columns")
        .striped(true)
        .num_columns(5)
        .spacing(vec2(8.0, 4.0))
        .show(ui, |ui| {
            ui.strong(tr("Output"));
            ui.strong(tr("Expression"));
            ui.strong(tr("Alias"));
            ui.strong(tr("Aggregate"));
            ui.label("");
            ui.end_row();
            for (i, item) in model.select.iter_mut().enumerate() {
                ui.checkbox(&mut item.visible, "");
                ui.horizontal(|ui| {
                    let before = item.expr.clone();
                    expr_field(ui, &mut item.expr, refs, 220.0);
                    changed |= before != item.expr;
                });
                ui.add(egui::TextEdit::singleline(&mut item.alias).desired_width(120.0));
                egui::ComboBox::from_id_salt(("dbiji_qb_agg", i))
                    .selected_text(item.aggregate.map(|a| a.label()).unwrap_or("–"))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut item.aggregate, None, "–");
                        for a in Aggregate::ALL {
                            ui.selectable_value(&mut item.aggregate, Some(a), a.label());
                        }
                    });
                ui.horizontal(|ui| row_buttons(ui, i, &mut action));
                ui.end_row();
            }
        });
    if action.is_some() {
        changed = true;
    }
    apply_row_action(&mut model.select, action);
    if ui
        .button((dbiji_icons::ic(icon::PLUS), tr("Add expression")))
        .clicked()
    {
        model.select.push(QbSelectItem::default());
    }
    if changed {
        model.sync_selected_flags();
    }
}

fn conditions_grid(ui: &mut Ui, salt: &str, list: &mut Vec<QbCondition>, refs: &[String]) {
    let mut action = None;
    egui::Grid::new(salt)
        .striped(true)
        .num_columns(5)
        .spacing(vec2(8.0, 4.0))
        .show(ui, |ui| {
            ui.label("");
            ui.strong(tr("Expression"));
            ui.strong(tr("Operator"));
            ui.strong(tr("Value"));
            ui.label("");
            ui.end_row();
            for (i, c) in list.iter_mut().enumerate() {
                if i == 0 {
                    ui.weak(tr("where"));
                } else {
                    egui::ComboBox::from_id_salt((salt, "conn", i))
                        .width(60.0)
                        .selected_text(c.connector.sql())
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut c.connector, Connector::And, "AND");
                            ui.selectable_value(&mut c.connector, Connector::Or, "OR");
                        });
                }
                ui.horizontal(|ui| expr_field(ui, &mut c.expr, refs, 200.0));
                egui::ComboBox::from_id_salt((salt, "op", i))
                    .width(100.0)
                    .selected_text(if c.operator.is_empty() {
                        tr("(expression)")
                    } else {
                        c.operator.as_str()
                    })
                    .show_ui(ui, |ui| {
                        for op in OPERATORS {
                            let label = if op.is_empty() {
                                tr("(expression)")
                            } else {
                                op
                            };
                            ui.selectable_value(&mut c.operator, op.to_string(), label);
                        }
                    });
                let needs_value = !matches!(c.operator.as_str(), "IS NULL" | "IS NOT NULL" | "");
                ui.add_enabled(
                    needs_value,
                    egui::TextEdit::singleline(&mut c.value)
                        .desired_width(180.0)
                        .hint_text(tr("'text', 42, a.column")),
                );
                ui.horizontal(|ui| row_buttons(ui, i, &mut action));
                ui.end_row();
            }
        });
    apply_row_action(list, action);
    if ui
        .button((dbiji_icons::ic(icon::PLUS), tr("Add condition")))
        .clicked()
    {
        list.push(QbCondition::new(
            refs.first().cloned().unwrap_or_default(),
            "=",
            "",
        ));
    }
}

fn grouping(ui: &mut Ui, model: &mut QueryModel, refs: &[String]) {
    ui.weak(tr("Columns without an aggregate are grouped automatically when the select list contains aggregates."));
    let mut action = None;
    egui::Grid::new("dbiji_qb_group")
        .striped(true)
        .num_columns(2)
        .spacing(vec2(8.0, 4.0))
        .show(ui, |ui| {
            for (i, g) in model.group_by.iter_mut().enumerate() {
                ui.horizontal(|ui| expr_field(ui, g, refs, 240.0));
                ui.horizontal(|ui| row_buttons(ui, i, &mut action));
                ui.end_row();
            }
        });
    apply_row_action(&mut model.group_by, action);
    if ui
        .button((dbiji_icons::ic(icon::PLUS), tr("Add group by expression")))
        .clicked()
    {
        model
            .group_by
            .push(refs.first().cloned().unwrap_or_default());
    }
    let effective = {
        let d = GenericDialect::new();
        model.effective_group_by(&d)
    };
    if !effective.is_empty() {
        ui.weak(trf("Effective GROUP BY: {}", &[&effective.join(", ")]));
    }
    ui.separator();
    ui.strong(tr("Having"));
    conditions_grid(ui, "dbiji_qb_having", &mut model.having, refs);
}

fn order_grid(ui: &mut Ui, model: &mut QueryModel, refs: &[String]) {
    let mut action = None;
    egui::Grid::new("dbiji_qb_order")
        .striped(true)
        .num_columns(3)
        .spacing(vec2(8.0, 4.0))
        .show(ui, |ui| {
            for (i, (expr, desc)) in model.order_by.iter_mut().enumerate() {
                ui.horizontal(|ui| expr_field(ui, expr, refs, 240.0));
                let label = if *desc {
                    format!("{} DESC", icon::SORT_DESCENDING)
                } else {
                    format!("{} ASC", icon::SORT_ASCENDING)
                };
                if ui
                    .button(label)
                    .on_hover_text(tr("Toggle sort direction"))
                    .clicked()
                {
                    *desc = !*desc;
                }
                ui.horizontal(|ui| row_buttons(ui, i, &mut action));
                ui.end_row();
            }
        });
    apply_row_action(&mut model.order_by, action);
    if ui
        .button((dbiji_icons::ic(icon::PLUS), tr("Add sort expression")))
        .clicked()
    {
        model
            .order_by
            .push((refs.first().cloned().unwrap_or_default(), false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::model::tests::{customers, orders};
    use crate::test_harness::Harness;

    fn row_center(model: &QueryModel, state: &QbViewState, alias: &str, column: &str) -> Pos2 {
        let t = model.table(alias).expect("table");
        let y = t.column_y(column).expect("column");
        let view = ViewTransform::new(state.zoom, state.pan);
        view.to_screen(state.last_canvas.min, pos2(t.rect().center().x, y))
    }

    #[test]
    fn canvas_interactions_edit_the_model() {
        let mut model = QueryModel::default();
        model.add_table(&customers());
        model.add_table(&orders());
        model.joins.clear();
        let mut state = QbViewState::default();
        let mut h = Harness::new(1300.0, 900.0);
        let mut events = Vec::new();
        let mut sql = String::new();
        let mut ui_fn = |ui: &mut Ui| {
            let r = show_query_builder(ui, &mut model, &mut state);
            events.extend(r.events);
            sql = r.sql;
        };
        h.idle(3, &mut ui_fn);
        drop(ui_fn);
        assert!(sql.contains("CROSS JOIN"), "{sql}");

        // click a row: the column is selected
        let p = row_center(&model, &state, "c", "name");
        let mut ui_fn =
            |ui: &mut Ui| events.extend(show_query_builder(ui, &mut model, &mut state).events);
        h.click(p, &mut ui_fn);
        drop(ui_fn);
        assert!(model.table("c").unwrap().columns[1].selected);
        assert_eq!(model.select.len(), 1);
        assert!(events.contains(&QbEvent::Changed));

        // drag a column onto another table's column: join
        let from = row_center(&model, &state, "o", "customer_id");
        let to = row_center(&model, &state, "c", "id");
        let mut ui_fn =
            |ui: &mut Ui| events.extend(show_query_builder(ui, &mut model, &mut state).events);
        h.drag(from, to, &mut ui_fn);
        drop(ui_fn);
        assert_eq!(model.joins.len(), 1, "{:?}", model.joins);
        assert_eq!(
            (
                model.joins[0].left_alias.as_str(),
                model.joins[0].right_alias.as_str()
            ),
            ("o", "c")
        );
        let sql = model.to_sql(state.dialect.as_ref());
        assert!(sql.contains("INNER JOIN"), "{sql}");

        // click the join label: kind cycles to LEFT
        let pts = join_points(&model, &model.joins[0]).unwrap();
        let label = ViewTransform::new(state.zoom, state.pan)
            .to_screen(state.last_canvas.min, join_label_pos(&pts).unwrap());
        let mut ui_fn =
            |ui: &mut Ui| events.extend(show_query_builder(ui, &mut model, &mut state).events);
        h.click(label, &mut ui_fn);
        drop(ui_fn);
        assert_eq!(model.joins[0].kind, JoinKind::Left);

        // every tab renders
        for tab in QbTab::ALL {
            state.tab = tab;
            let mut ui_fn = |ui: &mut Ui| {
                show_query_builder(ui, &mut model, &mut state);
            };
            h.idle(2, &mut ui_fn);
        }

        // the close button removes a table
        let t = model.table("o").unwrap().rect();
        let close = ViewTransform::new(state.zoom, state.pan).to_screen(
            state.last_canvas.min,
            pos2(t.right() - 10.0, t.top() + 10.0),
        );
        let mut ui_fn =
            |ui: &mut Ui| events.extend(show_query_builder(ui, &mut model, &mut state).events);
        h.click(close, &mut ui_fn);
        drop(ui_fn);
        assert!(model.table("o").is_none());
        assert!(model.joins.is_empty());
    }
}
