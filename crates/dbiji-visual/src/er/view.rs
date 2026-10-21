//! Interactive ER diagram canvas.
//!
//! Rendering does its own world -> screen transform so text stays crisp at
//! every zoom level, culls everything outside the viewport and lowers the
//! level of detail when zoomed out (no row text below ~46 %, row skeletons
//! below that, header bands only for overviews). Interaction: pan by
//! dragging the background, middle button or scrolling; zoom with
//! cmd/ctrl + scroll, pinch or the toolbar; drag boxes (multi-selection) and
//! notes; shift + drag selects with a rubber band; right click opens context
//! menus; in edit mode dragging a column onto another table proposes a
//! foreign key.

use super::diagram::{ColumnDisplay, ErDiagram, ErNote, Notation};
use super::export::{MarkerShape, marker_segments, unique_child};
use super::geometry::{
    COMMENT_FONT, COMMENT_HEIGHT, CORNER_RADIUS, HEADER_FONT, HEADER_HEIGHT, PAD_X, ROW_FONT,
    ROW_HEIGHT, SCHEMA_FONT, TableGeometry,
};
use super::model::{DiffStatus, ErRelation, ErTable, ErTableKind};
use super::scene::{ErScene, SceneTable};
use crate::common::colors;
use crate::common::paint::{self, font_px};
use crate::common::text::text_width;
use crate::common::view::{ViewTransform, distance_to_polyline, handle_pan_zoom, round_pos};
use dbiji_core::{tr, trf};
use egui::{
    Align2, Color32, CornerRadius, CursorIcon, Id, Key, Modifiers, Painter, PointerButton, Pos2,
    Rect, Response, Sense, Stroke, StrokeKind, TextEdit, Ui, Vec2, pos2, vec2,
};
use egui_phosphor::regular as icon;
use std::collections::BTreeSet;

/// Smallest and largest zoom factor.
pub const ZOOM_RANGE: (f32, f32) = (0.05, 4.0);

/// What the pointer is over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErHover {
    /// A table box (header or empty area).
    Table(String),
    /// A column row of a table.
    Column {
        /// Table key.
        table: String,
        /// Column name.
        column: String,
    },
    /// A relation line (index into `ErModel::relations`).
    Relation(usize),
    /// A note (index into `ErDiagram::notes`).
    Note(usize),
}

impl ErHover {
    /// Table key for table and column hovers.
    pub fn table(&self) -> Option<&str> {
        match self {
            ErHover::Table(t) | ErHover::Column { table: t, .. } => Some(t),
            _ => None,
        }
    }
}

/// Drag operation in progress.
#[derive(Debug, Clone, PartialEq)]
pub enum ErDrag {
    /// Panning the view.
    Pan,
    /// Moving the listed tables.
    MoveTables {
        /// Keys of the moved tables.
        keys: Vec<String>,
        /// The pointer actually moved them.
        moved: bool,
    },
    /// Moving a note.
    MoveNote(usize),
    /// Resizing a note with its corner handle.
    ResizeNote(usize),
    /// Rubber band selection (diagram coordinates).
    Select {
        /// Where the drag started.
        origin: Pos2,
        /// Current pointer position.
        current: Pos2,
    },
    /// Edit mode: dragging from a column to create a foreign key.
    Connect {
        /// Child table key.
        from_table: String,
        /// FK column.
        from_column: String,
        /// Current pointer position (diagram coordinates).
        current: Pos2,
    },
}

/// Something the application has to act on.
#[derive(Debug, Clone, PartialEq)]
pub enum ErEvent {
    /// Open the object editor of a table (double click / context menu).
    OpenTable(String),
    /// Open the data editor of a table.
    ViewData(String),
    /// Show DDL of the listed tables.
    GenerateDdl(Vec<String>),
    /// Tables were removed from the diagram (not from the database).
    RemoveFromDiagram(Vec<String>),
    /// Add the tables related to this one (the app fetches their
    /// definitions and calls [`ErDiagram::add_model`]).
    AddRelatedTables(String),
    /// Tables were moved (positions already updated in the diagram).
    TableMoved(Vec<String>),
    /// Selected table keys changed.
    SelectionChanged(Vec<String>),
    /// Display settings changed (diagram should be saved).
    SettingsChanged,
    /// Notes were added, edited, moved or deleted.
    NotesChanged,
    /// The user asked to export the diagram as an image.
    ExportRequested,
    /// Edit mode: create a new table at this diagram position.
    CreateTable {
        /// Diagram coordinates where the table should appear.
        pos: Pos2,
    },
    /// Edit mode: open the table designer.
    EditTable(String),
    /// Edit mode: drop the listed tables (after a DDL preview).
    DeleteTables(Vec<String>),
    /// Edit mode: add a column to the table.
    AddColumn(String),
    /// Edit mode: create a foreign key (child -> parent).
    CreateRelation {
        /// Referencing table.
        from_table: String,
        /// FK columns.
        from_columns: Vec<String>,
        /// Referenced table.
        to_table: String,
        /// Referenced columns (the dropped-on column, or the primary key).
        to_columns: Vec<String>,
    },
    /// Edit mode: drop this foreign key.
    DeleteRelation(ErRelation),
    /// Edit mode: edit this foreign key.
    EditRelation(ErRelation),
}

/// Result of [`show_er`].
#[derive(Debug, Clone, Default)]
pub struct ErResponse {
    /// Events produced this frame.
    pub events: Vec<ErEvent>,
    /// Element under the pointer.
    pub hovered: Option<ErHover>,
    /// Response of the canvas (for focus, extra context menus ...).
    pub response: Option<Response>,
}

#[derive(Debug, Clone, PartialEq)]
enum ContextTarget {
    Table(String),
    Relation(usize),
    Note(usize),
    Background(Pos2),
}

/// View state of an ER canvas (keep one per open diagram).
#[derive(Debug, Clone)]
pub struct ErViewState {
    /// Zoom factor (1 = 100 %).
    pub zoom: f32,
    /// Screen offset of the diagram origin from the canvas corner.
    pub pan: Vec2,
    /// Selected table keys.
    pub selection: BTreeSet<String>,
    /// Selected relation (index into `ErModel::relations`).
    pub selected_relation: Option<usize>,
    /// The relation selected at that index (a refreshed model may hold another one there).
    selected_relation_value: Option<ErRelation>,
    /// Selected note (index into `ErDiagram::notes`).
    pub selected_note: Option<usize>,
    /// Element under the pointer (updated every frame).
    pub hover: Option<ErHover>,
    /// Drag in progress.
    pub drag: Option<ErDrag>,
    /// Schema editing mode (create/edit/delete tables and foreign keys).
    pub edit_mode: bool,
    /// Show the built-in toolbar above the canvas.
    pub show_toolbar: bool,
    /// Show the overview map in the corner.
    pub show_minimap: bool,
    /// Zoom to fit on the next frame (set initially).
    pub fit_pending: bool,
    context: Option<ContextTarget>,
    editing_note: Option<usize>,
    last_canvas: Rect,
    minimap_rect: Option<Rect>,
    scene_cache: Option<(u64, ErScene)>,
}

impl Default for ErViewState {
    fn default() -> Self {
        ErViewState {
            zoom: 1.0,
            pan: vec2(20.0, 20.0),
            selection: BTreeSet::new(),
            selected_relation: None,
            selected_relation_value: None,
            selected_note: None,
            hover: None,
            drag: None,
            edit_mode: false,
            show_toolbar: true,
            show_minimap: true,
            fit_pending: true,
            context: None,
            editing_note: None,
            last_canvas: Rect::NOTHING,
            minimap_rect: None,
            scene_cache: None,
        }
    }
}

impl ErViewState {
    fn view(&self) -> ViewTransform {
        ViewTransform::new(self.zoom, self.pan)
    }

    fn set_view(&mut self, v: ViewTransform) {
        self.zoom = v.zoom;
        self.pan = v.pan;
    }

    /// Requests a zoom-to-fit on the next frame.
    pub fn zoom_to_fit(&mut self) {
        self.fit_pending = true;
    }

    /// Centres the view on a table and selects it (e.g. from a search box).
    pub fn reveal(&mut self, diagram: &ErDiagram, key: &str) {
        if let Some(r) = diagram.table_rect(key) {
            if self.last_canvas.is_positive() {
                self.pan = self.last_canvas.size() * 0.5 - r.center().to_vec2() * self.zoom;
            }
            self.selection = BTreeSet::from([key.to_string()]);
        }
    }
}

/// Theme dependent colors.
struct Palette {
    canvas: Color32,
    grid: Color32,
    box_fill: Color32,
    box_stroke: Stroke,
    header_table: Color32,
    header_view: Color32,
    text: Color32,
    weak: Color32,
    strong: Color32,
    line: Color32,
    highlight: Color32,
    selection: Stroke,
    row_hover: Color32,
    pk: Color32,
    fk: Color32,
    note_fill: Color32,
    note_stroke: Color32,
}

impl Palette {
    fn new(v: &egui::Visuals) -> Self {
        let dark = v.dark_mode;
        Palette {
            canvas: v.extreme_bg_color,
            grid: v
                .widgets
                .noninteractive
                .bg_stroke
                .color
                .gamma_multiply(0.45),
            box_fill: v.window_fill,
            box_stroke: Stroke::new(1.0, v.widgets.noninteractive.bg_stroke.color),
            header_table: colors::tint(
                v,
                colors::categorical(0, dark),
                if dark { 0.22 } else { 0.14 },
            ),
            header_view: colors::tint(
                v,
                colors::categorical(6, dark),
                if dark { 0.22 } else { 0.14 },
            ),
            text: v.text_color(),
            weak: v.weak_text_color(),
            strong: v.strong_text_color(),
            line: v.text_color().gamma_multiply(0.55),
            highlight: v.selection.stroke.color,
            selection: Stroke::new(2.0, v.selection.stroke.color),
            row_hover: v.selection.bg_fill.gamma_multiply(0.35),
            pk: colors::categorical(3, dark),
            fk: colors::categorical(0, dark),
            note_fill: colors::tint(
                v,
                Color32::from_rgb(0xfa, 0xd3, 0x4a),
                if dark { 0.20 } else { 0.32 },
            ),
            note_stroke: colors::tint(
                v,
                Color32::from_rgb(0xd9, 0xb1, 0x2a),
                if dark { 0.45 } else { 0.6 },
            ),
        }
    }

    fn header(&self, v: &egui::Visuals, kind: ErTableKind, status: Option<DiffStatus>) -> Color32 {
        let amount = if v.dark_mode { 0.32 } else { 0.30 };
        match status {
            Some(DiffStatus::Added) => colors::tint(v, colors::STATUS_GOOD, amount),
            Some(DiffStatus::Removed) => colors::tint(v, colors::STATUS_CRITICAL, amount),
            Some(DiffStatus::Modified) => colors::tint(v, colors::STATUS_WARNING, amount + 0.05),
            _ if kind == ErTableKind::View => self.header_view,
            _ => self.header_table,
        }
    }
}

fn status_stroke(status: Option<DiffStatus>) -> Option<Color32> {
    match status {
        Some(DiffStatus::Added) => Some(colors::STATUS_GOOD),
        Some(DiffStatus::Removed) => Some(colors::STATUS_CRITICAL),
        Some(DiffStatus::Modified) => Some(colors::STATUS_SERIOUS),
        _ => None,
    }
}

/// Shows the diagram canvas (with an optional toolbar) filling the
/// available space of `ui`. Use `ui.push_id` when showing several diagrams
/// in the same parent.
pub fn show_er(ui: &mut Ui, diagram: &mut ErDiagram, state: &mut ErViewState) -> ErResponse {
    let mut events = Vec::new();
    let base_id = ui.id().with("dbiji_er");
    if state.show_toolbar {
        toolbar(ui, diagram, state, &mut events);
    }
    let rect = ui.available_rect_before_wrap();
    let rect = Rect::from_min_size(rect.min, rect.size().max(vec2(64.0, 64.0)));
    let response = ui.interact(rect, base_id.with("canvas"), Sense::click_and_drag());
    ui.advance_cursor_after_rect(rect);
    // Early frames can report a provisional size; only fit once it is stable.
    let size_stable = state.last_canvas.size() == rect.size();
    state.last_canvas = rect;
    let origin = rect.min;
    let pal = Palette::new(ui.visuals());

    // keep indices valid after external model edits
    if state
        .selected_relation
        .is_some_and(|i| diagram.model.relations.get(i) != state.selected_relation_value.as_ref())
    {
        state.selected_relation = None;
    }
    if state
        .selected_note
        .is_some_and(|i| i >= diagram.notes.len())
    {
        state.selected_note = None;
    }
    if state.editing_note.is_some_and(|i| i >= diagram.notes.len()) {
        state.editing_note = None;
    }

    let mut scene = take_scene(state, diagram);
    if state.fit_pending {
        if size_stable {
            fit(state, diagram, &scene, rect.size());
            state.fit_pending = false;
        } else {
            ui.painter_at(rect).rect_filled(rect, 0.0, pal.canvas);
            ui.ctx().request_repaint();
            state.scene_cache = Some((scene_key(diagram), scene));
            return ErResponse {
                events,
                hovered: None,
                response: Some(response),
            };
        }
    }

    // --- navigation
    let mut view = state.view();
    if handle_pan_zoom(ui, &response, &mut view, origin, ZOOM_RANGE, false).changed {
        state.set_view(view);
    }

    let pointer = ui.input(|i| i.pointer.latest_pos());
    let in_minimap = |p: Pos2| state.minimap_rect.is_some_and(|m| m.contains(p));
    let hover_pos =
        pointer.filter(|p| rect.contains(*p) && !in_minimap(*p) && response.contains_pointer());
    let view = state.view();
    let draw_order = draw_order(&scene, diagram, state);
    state.hover = if state.drag.is_some() && !matches!(state.drag, Some(ErDrag::Connect { .. })) {
        state.hover.clone()
    } else {
        hover_pos.and_then(|p| {
            hit_test(
                &scene,
                diagram,
                &draw_order,
                view.to_world(origin, p),
                view.zoom,
            )
        })
    };
    let selection_before = state.selection.clone();

    // --- drags
    // tables moved by a drag this frame (cheap partial scene update), or a
    // finished drag (full rebuild so other routes can avoid the new spot)
    let mut moved_tables: Option<Vec<String>> = None;
    let mut full_rebuild = false;
    if response.drag_started_by(PointerButton::Primary) {
        let press = ui.input(|i| i.pointer.press_origin()).unwrap_or(origin);
        let world = view.to_world(origin, press);
        let mods = ui.input(|i| i.modifiers);
        let hit = hit_test(&scene, diagram, &draw_order, world, view.zoom);
        state.drag = Some(match hit {
            Some(ErHover::Note(i)) => {
                state.selected_note = Some(i);
                let corner = diagram.notes.get(i).map(|n| n.rect().max).unwrap_or(world);
                if (corner - world).length() * view.zoom < 14.0 {
                    ErDrag::ResizeNote(i)
                } else {
                    ErDrag::MoveNote(i)
                }
            }
            Some(ErHover::Column { table, column }) if state.edit_mode && !mods.shift => {
                ErDrag::Connect {
                    from_table: table,
                    from_column: column,
                    current: world,
                }
            }
            Some(ErHover::Table(key)) | Some(ErHover::Column { table: key, .. }) => {
                if !state.selection.contains(&key) {
                    if !(mods.shift || mods.command) {
                        state.selection.clear();
                    }
                    state.selection.insert(key.clone());
                }
                ErDrag::MoveTables {
                    keys: state.selection.iter().cloned().collect(),
                    moved: false,
                }
            }
            _ if mods.shift => ErDrag::Select {
                origin: world,
                current: world,
            },
            _ => ErDrag::Pan,
        });
    }
    if response.dragged_by(PointerButton::Primary) {
        let delta = response.drag_delta();
        let world_delta = delta / view.zoom.max(1e-3);
        let pointer_world = pointer.map(|p| view.to_world(origin, p));
        match &mut state.drag {
            Some(ErDrag::Pan) => state.pan += delta,
            Some(ErDrag::MoveTables { keys, moved }) => {
                if delta != Vec2::ZERO {
                    for k in keys.iter() {
                        if let Some(p) = diagram.positions.get_mut(k) {
                            *p += world_delta;
                        }
                    }
                    *moved = true;
                    moved_tables = Some(keys.clone());
                }
            }
            Some(ErDrag::MoveNote(i)) => {
                if let Some(n) = diagram.notes.get_mut(*i) {
                    n.pos += world_delta;
                }
            }
            Some(ErDrag::ResizeNote(i)) => {
                if let Some(n) = diagram.notes.get_mut(*i) {
                    n.size = (n.size + world_delta).max(vec2(60.0, 30.0));
                }
            }
            Some(ErDrag::Select { current, .. }) | Some(ErDrag::Connect { current, .. }) => {
                if let Some(p) = pointer_world {
                    *current = p;
                }
            }
            None => {}
        }
        if matches!(state.drag, Some(ErDrag::MoveTables { .. })) {
            ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        }
    }
    if response.drag_stopped() {
        match state.drag.take() {
            Some(ErDrag::MoveTables { keys, moved: true }) => {
                for k in &keys {
                    if let Some(p) = diagram.positions.get_mut(k) {
                        *p = round_pos(*p);
                    }
                }
                events.push(ErEvent::TableMoved(keys));
                full_rebuild = true;
            }
            Some(ErDrag::MoveNote(_)) | Some(ErDrag::ResizeNote(_)) => {
                events.push(ErEvent::NotesChanged)
            }
            Some(ErDrag::Select { origin: o, current }) => {
                let band = Rect::from_two_pos(o, current);
                for st in &scene.tables {
                    if band.intersects(st.rect)
                        && let Some(t) = diagram.model.tables.get(st.table)
                    {
                        state.selection.insert(t.key.clone());
                    }
                }
            }
            Some(ErDrag::Connect {
                from_table,
                from_column,
                current,
            }) => {
                if let Some(target) = hit_test(&scene, diagram, &draw_order, current, view.zoom) {
                    let to_columns = match &target {
                        ErHover::Column { column, .. } => Some(vec![column.clone()]),
                        // The whole primary key, as many columns as the foreign key has (one).
                        ErHover::Table(k) => diagram
                            .model
                            .table(k)
                            .map(|t| t.pk_columns())
                            .filter(|pk| pk.len() == 1),
                        _ => None,
                    };
                    if let (Some(to_table), Some(to_columns)) =
                        (target.table().map(str::to_string), to_columns)
                    {
                        events.push(ErEvent::CreateRelation {
                            from_table,
                            from_columns: vec![from_column],
                            to_table,
                            to_columns,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    // --- clicks
    let mods = ui.input(|i| i.modifiers);
    if response.clicked() {
        response.request_focus();
        let hit = hover_pos.and_then(|p| {
            hit_test(
                &scene,
                diagram,
                &draw_order,
                view.to_world(origin, p),
                view.zoom,
            )
        });
        state.selected_relation = None;
        state.selected_note = None;
        match hit {
            Some(ErHover::Table(k)) | Some(ErHover::Column { table: k, .. }) => {
                if mods.shift || mods.command {
                    if !state.selection.remove(&k) {
                        state.selection.insert(k);
                    }
                } else {
                    state.selection = BTreeSet::from([k]);
                }
            }
            Some(ErHover::Relation(i)) => {
                state.selected_relation = Some(i);
                state.selected_relation_value = diagram.model.relations.get(i).cloned();
                if !(mods.shift || mods.command) {
                    state.selection.clear();
                }
            }
            Some(ErHover::Note(i)) => state.selected_note = Some(i),
            None => {
                if !(mods.shift || mods.command) {
                    state.selection.clear();
                }
            }
        }
        if state.editing_note.is_some() {
            state.editing_note = None;
            events.push(ErEvent::NotesChanged);
        }
    }
    if response.double_clicked() {
        match state.hover.clone() {
            Some(ErHover::Table(k)) | Some(ErHover::Column { table: k, .. }) => {
                events.push(if state.edit_mode {
                    ErEvent::EditTable(k)
                } else {
                    ErEvent::OpenTable(k)
                });
            }
            Some(ErHover::Note(i)) => state.editing_note = Some(i),
            Some(ErHover::Relation(i)) if state.edit_mode => {
                if let Some(r) = diagram.model.relations.get(i) {
                    events.push(ErEvent::EditRelation(r.clone()));
                }
            }
            _ => {}
        }
    }
    if response.secondary_clicked() {
        let world = hover_pos
            .map(|p| view.to_world(origin, p))
            .unwrap_or(Pos2::ZERO);
        state.context = Some(match state.hover.clone() {
            Some(ErHover::Table(k)) | Some(ErHover::Column { table: k, .. }) => {
                if !state.selection.contains(&k) {
                    state.selection = BTreeSet::from([k.clone()]);
                }
                ContextTarget::Table(k)
            }
            Some(ErHover::Relation(i)) => ContextTarget::Relation(i),
            Some(ErHover::Note(i)) => ContextTarget::Note(i),
            None => ContextTarget::Background(world),
        });
    }

    // --- keyboard (when the canvas has focus)
    if response.has_focus() && state.editing_note.is_none() {
        keyboard(ui, diagram, state, &mut events);
    }

    // --- context menu
    let mut action: Option<MenuAction> = None;
    if let Some(target) = state.context.clone() {
        let edit = state.edit_mode;
        let selection = state.selection.clone();
        response.context_menu(|ui| {
            action = context_menu_ui(ui, &target, diagram, &selection, edit);
        });
    }
    if let Some(a) = action {
        apply_action(a, diagram, state, &mut events);
        scene = ErScene::build(diagram);
    } else if full_rebuild {
        scene = ErScene::build(diagram);
    } else if let Some(keys) = moved_tables {
        scene.update_moved(diagram, &keys);
    }

    if state.selection != selection_before {
        events.push(ErEvent::SelectionChanged(
            state.selection.iter().cloned().collect(),
        ));
    }

    // --- painting
    let painter = ui.painter_at(rect);
    let view = state.view();
    paint_canvas(ui, &painter, rect, diagram, state, &scene, &pal, view);

    // note editor
    if let Some(i) = state.editing_note
        && let Some(note) = diagram.notes.get_mut(i)
    {
        let r = view.rect_to_screen(origin, note.rect()).shrink(2.0);
        if r.intersects(rect) {
            let edit = ui.put(
                r,
                TextEdit::multiline(&mut note.text)
                    .frame(egui::Frame::NONE)
                    .desired_width(r.width()),
            );
            if !edit.has_focus() && !edit.lost_focus() {
                edit.request_focus();
            }
            if edit.lost_focus() {
                state.editing_note = None;
                events.push(ErEvent::NotesChanged);
            }
        }
    }

    // minimap
    state.minimap_rect = None;
    if state.show_minimap && !scene.tables.is_empty() {
        minimap(ui, &painter, rect, base_id, &scene, diagram, state, &pal);
    }

    // tooltip
    if state.drag.is_none()
        && let Some(h) = state.hover.clone()
    {
        let r = response.clone();
        r.on_hover_ui_at_pointer(|ui| hover_tooltip(ui, diagram, &h));
    }
    if matches!(
        state.hover,
        Some(ErHover::Table(_)) | Some(ErHover::Column { .. })
    ) && state.drag.is_none()
    {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }

    state.scene_cache = Some((scene_key(diagram), scene));
    ErResponse {
        events,
        hovered: state.hover.clone(),
        response: Some(response),
    }
}

/// Content hash of everything the scene geometry depends on.
fn scene_key(diagram: &ErDiagram) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    diagram.settings.hash(&mut h);
    diagram.model.hash(&mut h);
    for (k, p) in &diagram.positions {
        k.hash(&mut h);
        p.x.to_bits().hash(&mut h);
        p.y.to_bits().hash(&mut h);
    }
    h.finish()
}

/// The cached scene when nothing changed since the last frame, otherwise a
/// freshly built one (routing with obstacle avoidance is not free).
fn take_scene(state: &mut ErViewState, diagram: &ErDiagram) -> ErScene {
    let key = scene_key(diagram);
    match state.scene_cache.take() {
        Some((k, scene)) if k == key => scene,
        _ => ErScene::build(diagram),
    }
}

fn fit(state: &mut ErViewState, diagram: &ErDiagram, scene: &ErScene, size: Vec2) {
    let mut bounds = scene.bounds();
    for n in &diagram.notes {
        bounds = Some(bounds.map_or(n.rect(), |b| b.union(n.rect())));
    }
    let mut v = state.view();
    match bounds {
        Some(b) => v.fit(size, b, 30.0, ZOOM_RANGE.0, 1.0),
        None => v = ViewTransform::default(),
    }
    state.set_view(v);
}

/// Tables in paint order: unselected first, selected on top.
fn draw_order(scene: &ErScene, diagram: &ErDiagram, state: &ErViewState) -> Vec<usize> {
    let selected = |st: &SceneTable| {
        diagram
            .model
            .tables
            .get(st.table)
            .map(|t| state.selection.contains(&t.key))
            .unwrap_or(false)
    };
    let mut order: Vec<usize> = (0..scene.tables.len())
        .filter(|&i| !selected(&scene.tables[i]))
        .collect();
    order.extend((0..scene.tables.len()).filter(|&i| selected(&scene.tables[i])));
    order
}

fn hit_test(
    scene: &ErScene,
    diagram: &ErDiagram,
    order: &[usize],
    world: Pos2,
    zoom: f32,
) -> Option<ErHover> {
    for (i, n) in diagram.notes.iter().enumerate().rev() {
        if n.rect().contains(world) {
            return Some(ErHover::Note(i));
        }
    }
    for &ti in order.iter().rev() {
        let Some(st) = scene.tables.get(ti) else {
            continue;
        };
        if !st.rect.contains(world) {
            continue;
        }
        let Some(t) = diagram.model.tables.get(st.table) else {
            continue;
        };
        let rel_y = world.y - st.rect.top();
        if let Some(ci) = st.geom.row_at(rel_y)
            && let Some(c) = t.columns.get(ci)
        {
            return Some(ErHover::Column {
                table: t.key.clone(),
                column: c.name.clone(),
            });
        }
        return Some(ErHover::Table(t.key.clone()));
    }
    let tolerance = 5.0 / zoom.max(1e-3);
    let mut best: Option<(f32, usize)> = None;
    for sr in &scene.relations {
        if !sr.route.bbox.expand(tolerance).contains(world) {
            continue;
        }
        let d = distance_to_polyline(world, &sr.route.points);
        if d <= tolerance && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, sr.relation));
        }
    }
    best.map(|(_, i)| ErHover::Relation(i))
}

// ---------------------------------------------------------------- toolbar

fn toolbar(
    ui: &mut Ui,
    diagram: &mut ErDiagram,
    state: &mut ErViewState,
    events: &mut Vec<ErEvent>,
) {
    ui.horizontal(|ui| {
        let center = if state.last_canvas.is_positive() {
            state.last_canvas.center()
        } else {
            Pos2::ZERO
        };
        let origin = if state.last_canvas.is_positive() {
            state.last_canvas.min
        } else {
            Pos2::ZERO
        };
        if ui
            .button(dbiji_icons::ic(icon::MAGNIFYING_GLASS_MINUS))
            .on_hover_text(tr("Zoom out"))
            .clicked()
        {
            let mut v = state.view();
            v.zoom_around(origin, center, 1.0 / 1.25, ZOOM_RANGE.0, ZOOM_RANGE.1);
            state.set_view(v);
        }
        if ui
            .button(format!("{:.0}%", state.zoom * 100.0))
            .on_hover_text(tr("Reset zoom to 100%"))
            .clicked()
        {
            let mut v = state.view();
            v.zoom_around(
                origin,
                center,
                1.0 / v.zoom.max(1e-3),
                ZOOM_RANGE.0,
                ZOOM_RANGE.1,
            );
            state.set_view(v);
        }
        if ui
            .button(dbiji_icons::ic(icon::MAGNIFYING_GLASS_PLUS))
            .on_hover_text(tr("Zoom in"))
            .clicked()
        {
            let mut v = state.view();
            v.zoom_around(origin, center, 1.25, ZOOM_RANGE.0, ZOOM_RANGE.1);
            state.set_view(v);
        }
        if ui
            .button(dbiji_icons::ic(icon::CORNERS_OUT))
            .on_hover_text(tr("Zoom to fit"))
            .clicked()
        {
            state.fit_pending = true;
        }
        ui.separator();
        let before = diagram.settings;
        egui::ComboBox::from_id_salt("dbiji_er_columns")
            .selected_text(tr(diagram.settings.columns.label()))
            .show_ui(ui, |ui| {
                for d in ColumnDisplay::ALL {
                    ui.selectable_value(&mut diagram.settings.columns, d, tr(d.label()));
                }
            })
            .response
            .on_hover_text(tr("Columns shown in the boxes"));
        ui.toggle_value(&mut diagram.settings.show_types, tr("Types"))
            .on_hover_text(tr("Show column types"));
        ui.toggle_value(&mut diagram.settings.show_comments, tr("Comments"))
            .on_hover_text(tr("Show table comments"));
        egui::ComboBox::from_id_salt("dbiji_er_notation")
            .selected_text(tr(diagram.settings.notation.label()))
            .show_ui(ui, |ui| {
                for n in [Notation::CrowsFoot, Notation::Simple] {
                    ui.selectable_value(&mut diagram.settings.notation, n, tr(n.label()));
                }
            });
        if diagram.settings != before {
            events.push(ErEvent::SettingsChanged);
        }
        ui.separator();
        if ui
            .button((dbiji_icons::ic(icon::TREE_STRUCTURE), tr("Auto layout")))
            .on_hover_text(tr("Arrange all tables automatically"))
            .clicked()
        {
            diagram.relayout();
            state.fit_pending = true;
            events.push(ErEvent::TableMoved(
                diagram.model.tables.iter().map(|t| t.key.clone()).collect(),
            ));
        }
        ui.toggle_value(&mut state.show_minimap, dbiji_icons::ic(icon::MAP_TRIFOLD))
            .on_hover_text(tr("Show overview map"));
        ui.toggle_value(
            &mut state.edit_mode,
            (dbiji_icons::ic(icon::PENCIL_SIMPLE), tr("Edit mode")),
        )
        .on_hover_text(tr(
            "Edit the schema from the diagram: create and change tables and foreign keys",
        ));
        if ui
            .button((dbiji_icons::ic(icon::EXPORT), tr("Export…")))
            .on_hover_text(tr("Export image"))
            .clicked()
        {
            events.push(ErEvent::ExportRequested);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.weak(trf(
                "{} tables, {} relations",
                &[&diagram.model.tables.len(), &diagram.model.relations.len()],
            ));
        });
    });
}

// ---------------------------------------------------------------- menus

#[derive(Debug, Clone)]
enum MenuAction {
    Emit(ErEvent),
    RemoveTables(Vec<String>),
    SelectRelated(String),
    SelectTables(Vec<String>),
    SelectAll,
    Fit,
    Relayout,
    AddNote(Pos2),
    EditNote(usize),
    DeleteNote(usize),
}

fn target_keys(key: &str, selection: &BTreeSet<String>) -> Vec<String> {
    if selection.contains(key) && selection.len() > 1 {
        selection.iter().cloned().collect()
    } else {
        vec![key.to_string()]
    }
}

fn context_menu_ui(
    ui: &mut Ui,
    target: &ContextTarget,
    diagram: &ErDiagram,
    selection: &BTreeSet<String>,
    edit_mode: bool,
) -> Option<MenuAction> {
    let mut action = None;
    let mut item = |ui: &mut Ui, text: String, a: MenuAction| {
        if ui.button(text).clicked() {
            action = Some(a);
            ui.close();
        }
    };
    match target {
        ContextTarget::Table(key) => {
            let keys = target_keys(key, selection);
            ui.label(egui::RichText::new(key.as_str()).strong());
            ui.separator();
            item(
                ui,
                format!("{} {}", icon::ARROW_SQUARE_OUT, tr("Open")),
                MenuAction::Emit(ErEvent::OpenTable(key.clone())),
            );
            item(
                ui,
                format!("{} {}", icon::TABLE, tr("View data")),
                MenuAction::Emit(ErEvent::ViewData(key.clone())),
            );
            item(
                ui,
                format!("{} {}", icon::CODE, tr("Generate DDL")),
                MenuAction::Emit(ErEvent::GenerateDdl(keys.clone())),
            );
            item(
                ui,
                format!("{} {}", icon::GRAPH, tr("Add related tables")),
                MenuAction::Emit(ErEvent::AddRelatedTables(key.clone())),
            );
            item(
                ui,
                format!("{} {}", icon::LINK, tr("Select related tables")),
                MenuAction::SelectRelated(key.clone()),
            );
            item(
                ui,
                format!("{} {}", icon::EYE_SLASH, tr("Remove from diagram")),
                MenuAction::RemoveTables(keys.clone()),
            );
            if edit_mode {
                ui.separator();
                item(
                    ui,
                    format!("{} {}", icon::PENCIL_SIMPLE, tr("Edit table…")),
                    MenuAction::Emit(ErEvent::EditTable(key.clone())),
                );
                item(
                    ui,
                    format!("{} {}", icon::PLUS, tr("Add column…")),
                    MenuAction::Emit(ErEvent::AddColumn(key.clone())),
                );
                item(
                    ui,
                    format!("{} {}", icon::TRASH, tr("Delete table…")),
                    MenuAction::Emit(ErEvent::DeleteTables(keys)),
                );
            }
        }
        ContextTarget::Relation(i) => {
            if let Some(r) = diagram.model.relations.get(*i) {
                ui.label(egui::RichText::new(r.name.as_str()).strong());
                ui.separator();
                item(
                    ui,
                    format!("{} {}", icon::LINK, tr("Select both tables")),
                    MenuAction::SelectTables(vec![r.from_table.clone(), r.to_table.clone()]),
                );
                if edit_mode {
                    item(
                        ui,
                        format!("{} {}", icon::PENCIL_SIMPLE, tr("Edit foreign key…")),
                        MenuAction::Emit(ErEvent::EditRelation(r.clone())),
                    );
                    item(
                        ui,
                        format!("{} {}", icon::TRASH, tr("Delete foreign key…")),
                        MenuAction::Emit(ErEvent::DeleteRelation(r.clone())),
                    );
                }
            }
        }
        ContextTarget::Note(i) => {
            item(
                ui,
                format!("{} {}", icon::PENCIL_SIMPLE, tr("Edit note")),
                MenuAction::EditNote(*i),
            );
            item(
                ui,
                format!("{} {}", icon::TRASH, tr("Delete note")),
                MenuAction::DeleteNote(*i),
            );
        }
        ContextTarget::Background(pos) => {
            item(
                ui,
                format!("{} {}", icon::CORNERS_OUT, tr("Zoom to fit")),
                MenuAction::Fit,
            );
            item(
                ui,
                format!("{} {}", icon::TREE_STRUCTURE, tr("Auto layout")),
                MenuAction::Relayout,
            );
            item(
                ui,
                format!("{} {}", icon::SELECTION_ALL, tr("Select all")),
                MenuAction::SelectAll,
            );
            item(
                ui,
                format!("{} {}", icon::NOTE, tr("Add note")),
                MenuAction::AddNote(*pos),
            );
            item(
                ui,
                format!("{} {}", icon::EXPORT, tr("Export image…")),
                MenuAction::Emit(ErEvent::ExportRequested),
            );
            if edit_mode {
                ui.separator();
                item(
                    ui,
                    format!("{} {}", icon::PLUS, tr("Create table here…")),
                    MenuAction::Emit(ErEvent::CreateTable { pos: *pos }),
                );
            }
        }
    }
    action
}

fn apply_action(
    action: MenuAction,
    diagram: &mut ErDiagram,
    state: &mut ErViewState,
    events: &mut Vec<ErEvent>,
) {
    state.context = None;
    match action {
        MenuAction::Emit(e) => events.push(e),
        MenuAction::RemoveTables(keys) => {
            let set: BTreeSet<String> = keys.iter().cloned().collect();
            diagram.remove_tables(&set);
            state.selection.retain(|k| !set.contains(k));
            state.selected_relation = None;
            events.push(ErEvent::RemoveFromDiagram(keys));
        }
        MenuAction::SelectRelated(key) => {
            let mut s = diagram.model.related_keys(&key);
            s.insert(key);
            state.selection = s;
        }
        MenuAction::SelectTables(keys) => state.selection = keys.into_iter().collect(),
        MenuAction::SelectAll => {
            state.selection = diagram.model.tables.iter().map(|t| t.key.clone()).collect()
        }
        MenuAction::Fit => state.fit_pending = true,
        MenuAction::Relayout => {
            diagram.relayout();
            state.fit_pending = true;
            events.push(ErEvent::TableMoved(
                diagram.model.tables.iter().map(|t| t.key.clone()).collect(),
            ));
        }
        MenuAction::AddNote(pos) => {
            diagram
                .notes
                .push(ErNote::new(String::new(), round_pos(pos)));
            state.editing_note = Some(diagram.notes.len() - 1);
            events.push(ErEvent::NotesChanged);
        }
        MenuAction::EditNote(i) => state.editing_note = Some(i),
        MenuAction::DeleteNote(i) => {
            if i < diagram.notes.len() {
                diagram.notes.remove(i);
                state.selected_note = None;
                state.editing_note = None;
                events.push(ErEvent::NotesChanged);
            }
        }
    }
}

fn keyboard(ui: &Ui, diagram: &mut ErDiagram, state: &mut ErViewState, events: &mut Vec<ErEvent>) {
    let (delete, escape, select_all) = ui.input_mut(|i| {
        let delete = i.consume_key(Modifiers::NONE, Key::Delete)
            || i.consume_key(Modifiers::NONE, Key::Backspace);
        let escape = i.consume_key(Modifiers::NONE, Key::Escape);
        let select_all = i.consume_key(Modifiers::COMMAND, Key::A);
        (delete, escape, select_all)
    });
    if escape {
        if matches!(
            state.drag,
            Some(ErDrag::Connect { .. }) | Some(ErDrag::Select { .. })
        ) {
            state.drag = None;
        } else {
            state.selection.clear();
            state.selected_relation = None;
            state.selected_note = None;
        }
    }
    if select_all {
        state.selection = diagram.model.tables.iter().map(|t| t.key.clone()).collect();
    }
    if delete {
        if let Some(i) = state.selected_note {
            if i < diagram.notes.len() {
                diagram.notes.remove(i);
                state.selected_note = None;
                events.push(ErEvent::NotesChanged);
            }
        } else if state.edit_mode {
            if let Some(r) = state
                .selected_relation
                .and_then(|i| diagram.model.relations.get(i))
            {
                events.push(ErEvent::DeleteRelation(r.clone()));
            } else if !state.selection.is_empty() {
                events.push(ErEvent::DeleteTables(
                    state.selection.iter().cloned().collect(),
                ));
            }
        }
    }
}

// ---------------------------------------------------------------- painting

#[allow(clippy::too_many_arguments)]
fn paint_canvas(
    ui: &Ui,
    painter: &Painter,
    rect: Rect,
    diagram: &ErDiagram,
    state: &ErViewState,
    scene: &ErScene,
    pal: &Palette,
    view: ViewTransform,
) {
    let origin = rect.min;
    let zoom = view.zoom;
    painter.rect_filled(rect, 0.0, pal.canvas);
    let visible = view
        .rect_to_world(origin, rect)
        .expand(40.0 / zoom.max(1e-3));

    // grid
    let step = 100.0;
    if step * zoom >= 24.0 {
        let stroke = Stroke::new(1.0, pal.grid);
        let mut x = (visible.left() / step).floor() * step;
        while x <= visible.right() {
            let sx = view.to_screen(origin, pos2(x, 0.0)).x;
            painter.vline(sx, rect.y_range(), stroke);
            x += step;
        }
        let mut y = (visible.top() / step).floor() * step;
        while y <= visible.bottom() {
            let sy = view.to_screen(origin, pos2(0.0, y)).y;
            painter.hline(rect.x_range(), sy, stroke);
            y += step;
        }
    }

    // highlight sets
    let hovered_table = state
        .hover
        .as_ref()
        .and_then(|h| h.table())
        .map(str::to_string);
    let hovered_relation = match &state.hover {
        Some(ErHover::Relation(i)) => Some(*i),
        _ => None,
    };
    let mut hot_relations: BTreeSet<usize> = BTreeSet::new();
    let mut hot_tables: BTreeSet<String> = BTreeSet::new();
    for (i, r) in diagram.model.relations.iter().enumerate() {
        let touches = |k: &str| hovered_table.as_deref() == Some(k) || state.selection.contains(k);
        if touches(&r.from_table)
            || touches(&r.to_table)
            || hovered_relation == Some(i)
            || state.selected_relation == Some(i)
        {
            hot_relations.insert(i);
            hot_tables.insert(r.from_table.clone());
            hot_tables.insert(r.to_table.clone());
        }
    }

    // relations: normal first, highlighted on top
    let detail = zoom >= 0.3;
    for pass in 0..2 {
        for sr in &scene.relations {
            let hot = hot_relations.contains(&sr.relation);
            if (pass == 0) == hot || !sr.route.bbox.intersects(visible) {
                continue;
            }
            let Some(r) = diagram.model.relations.get(sr.relation) else {
                continue;
            };
            let color = if hot { pal.highlight } else { pal.line };
            let width = if hot {
                2.0
            } else {
                (1.2 * zoom.sqrt()).clamp(0.8, 1.6)
            };
            let stroke = Stroke::new(width, color);
            let pts: Vec<Pos2> = sr
                .route
                .points
                .iter()
                .map(|p| view.to_screen(origin, *p))
                .collect();
            let dashed =
                diagram.settings.notation == Notation::CrowsFoot && !r.identifying && zoom >= 0.45;
            if dashed {
                paint::dashed_polyline(painter, &pts, stroke, 6.0, 4.0);
            } else {
                painter.line(pts, stroke);
            }
            if detail
                && let (Some(&s), Some(&e)) = (sr.route.points.first(), sr.route.points.last())
            {
                let shapes = marker_segments(
                    s,
                    sr.route.start_dir,
                    e,
                    sr.route.end_dir,
                    r,
                    diagram.settings.notation,
                    unique_child(diagram, r),
                );
                paint_markers(
                    painter,
                    &shapes,
                    view,
                    origin,
                    Stroke::new(width.max(1.2), color),
                    pal.box_fill,
                );
            }
        }
    }

    // tables
    let hovered_column = match &state.hover {
        Some(ErHover::Column { table, column }) => Some((table.as_str(), column.as_str())),
        _ => None,
    };
    let connect_source = match &state.drag {
        Some(ErDrag::Connect {
            from_table,
            from_column,
            ..
        }) => Some((from_table.as_str(), from_column.as_str())),
        _ => None,
    };
    let selected = |st: &SceneTable| {
        diagram
            .model
            .tables
            .get(st.table)
            .map(|t| state.selection.contains(&t.key))
            .unwrap_or(false)
    };
    let order: Vec<&SceneTable> = scene
        .tables
        .iter()
        .filter(|st| !selected(st))
        .chain(scene.tables.iter().filter(|st| selected(st)))
        .collect();
    for st in order {
        if !st.rect.intersects(visible) {
            continue;
        }
        let Some(t) = diagram.model.tables.get(st.table) else {
            continue;
        };
        let srect = view.rect_to_screen(origin, st.rect);
        let is_selected = state.selection.contains(&t.key);
        let is_hot =
            hot_tables.contains(&t.key) || hovered_table.as_deref() == Some(t.key.as_str());
        let row_highlight = hovered_column
            .filter(|(tk, _)| *tk == t.key)
            .or(connect_source.filter(|(tk, _)| *tk == t.key))
            .map(|(_, c)| c);
        paint_table(
            ui,
            painter,
            pal,
            t,
            &st.geom,
            srect,
            zoom,
            diagram,
            is_selected,
            is_hot,
            row_highlight,
        );
    }

    // notes
    for (i, n) in diagram.notes.iter().enumerate() {
        if !n.rect().intersects(visible) {
            continue;
        }
        let r = view.rect_to_screen(origin, n.rect());
        let radius = (4.0 * zoom).clamp(0.0, 8.0);
        painter.rect_filled(r, radius, pal.note_fill);
        let stroke = if state.selected_note == Some(i) {
            pal.selection
        } else {
            Stroke::new(1.0, pal.note_stroke)
        };
        painter.rect_stroke(r, radius, stroke, StrokeKind::Inside);
        if state.editing_note != Some(i)
            && let Some(px) = font_px(12.0, zoom)
        {
            let galley = painter.layout(
                n.text.clone(),
                egui::FontId::proportional(px),
                Color32::PLACEHOLDER,
                (r.width() - 12.0 * zoom).max(10.0),
            );
            let clip = painter.with_clip_rect(r.shrink(2.0).intersect(painter.clip_rect()));
            clip.galley(r.min + vec2(6.0, 5.0) * zoom, galley, pal.text);
        }
        // resize handle
        if zoom > 0.4 {
            let c = r.max;
            let s = Stroke::new(1.0, pal.note_stroke);
            painter.line_segment([c - vec2(10.0, 3.0), c - vec2(3.0, 10.0)], s);
            painter.line_segment([c - vec2(6.0, 3.0), c - vec2(3.0, 6.0)], s);
        }
    }

    // in-progress drags
    match &state.drag {
        Some(ErDrag::Select { origin: o, current }) => {
            let band = view.rect_to_screen(origin, Rect::from_two_pos(*o, *current));
            painter.rect_filled(band, 0.0, pal.selection.color.gamma_multiply(0.12));
            painter.rect_stroke(
                band,
                0.0,
                Stroke::new(1.0, pal.selection.color),
                StrokeKind::Inside,
            );
        }
        Some(ErDrag::Connect {
            from_table,
            from_column,
            current,
        }) => {
            if let Some(&ti) = scene.index.get(from_table)
                && let (Some(st), Some(t)) = (scene.tables.get(ti), diagram.model.table(from_table))
            {
                let y = st.rect.top() + st.geom.anchor_y(t, Some(from_column));
                let start_world = if current.x >= st.rect.center().x {
                    pos2(st.rect.right(), y)
                } else {
                    pos2(st.rect.left(), y)
                };
                let a = view.to_screen(origin, start_world);
                let b = view.to_screen(origin, *current);
                paint::dashed_polyline(painter, &[a, b], Stroke::new(2.0, pal.highlight), 6.0, 4.0);
                painter.circle_filled(b, 4.0, pal.highlight);
                if let Some(target) = hit_test(
                    scene,
                    diagram,
                    &draw_order(scene, diagram, state),
                    *current,
                    zoom,
                ) && let Some(k) = target.table()
                    && let Some(tr_rect) = scene
                        .index
                        .get(k)
                        .and_then(|&i| scene.tables.get(i))
                        .map(|s| s.rect)
                {
                    painter.rect_stroke(
                        view.rect_to_screen(origin, tr_rect).expand(3.0),
                        (CORNER_RADIUS * zoom).clamp(0.0, 12.0),
                        Stroke::new(2.0, pal.highlight),
                        StrokeKind::Outside,
                    );
                }
            }
        }
        _ => {}
    }

    if scene.tables.is_empty() && diagram.notes.is_empty() {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            tr("The diagram is empty"),
            egui::FontId::proportional(14.0),
            pal.weak,
        );
    }
    diff_legend(ui, painter, rect, diagram, pal);
}

/// Legend of the diff colours, shown when the diagram compares two schemas.
fn diff_legend(ui: &Ui, painter: &Painter, rect: Rect, diagram: &ErDiagram, pal: &Palette) {
    let mut counts = [0usize; 3];
    for t in &diagram.model.tables {
        match t.status {
            Some(DiffStatus::Added) => counts[0] += 1,
            Some(DiffStatus::Removed) => counts[1] += 1,
            Some(DiffStatus::Modified) => counts[2] += 1,
            _ => {}
        }
    }
    if !diagram.model.tables.iter().any(|t| t.status.is_some()) {
        return;
    }
    let v = ui.visuals();
    let items = [
        (DiffStatus::Added, counts[0]),
        (DiffStatus::Removed, counts[1]),
        (DiffStatus::Modified, counts[2]),
    ];
    let font = egui::FontId::proportional(12.0);
    let row_h = 18.0;
    let frame = Rect::from_min_size(
        rect.min + vec2(10.0, 10.0),
        vec2(150.0, row_h * items.len() as f32 + 10.0),
    );
    painter.rect_filled(frame, 4.0, pal.box_fill.gamma_multiply(0.95));
    painter.rect_stroke(frame, 4.0, pal.box_stroke, StrokeKind::Inside);
    for (i, (status, n)) in items.iter().enumerate() {
        let y = frame.top() + 5.0 + row_h * (i as f32 + 0.5);
        let swatch = Rect::from_center_size(pos2(frame.left() + 14.0, y), vec2(12.0, 12.0));
        painter.rect_filled(
            swatch,
            3.0,
            pal.header(v, ErTableKind::Table, Some(*status)),
        );
        painter.rect_stroke(
            swatch,
            3.0,
            Stroke::new(1.5, status_stroke(Some(*status)).unwrap_or(pal.weak)),
            StrokeKind::Inside,
        );
        painter.text(
            pos2(frame.left() + 28.0, y),
            Align2::LEFT_CENTER,
            format!("{} ({n})", tr(status.label())),
            font.clone(),
            pal.text,
        );
    }
}

fn paint_markers(
    painter: &Painter,
    shapes: &[MarkerShape],
    view: ViewTransform,
    origin: Pos2,
    stroke: Stroke,
    fill: Color32,
) {
    let zoom = view.zoom;
    for s in shapes {
        match s {
            MarkerShape::Line(a, b) => {
                painter.line_segment(
                    [view.to_screen(origin, *a), view.to_screen(origin, *b)],
                    stroke,
                );
            }
            MarkerShape::Circle(c, r) => {
                painter.circle(view.to_screen(origin, *c), r * zoom, fill, stroke);
            }
            MarkerShape::Arrow(points) => {
                let pts: Vec<Pos2> = points.iter().map(|p| view.to_screen(origin, *p)).collect();
                painter.add(egui::Shape::convex_polygon(pts, stroke.color, Stroke::NONE));
            }
            MarkerShape::Dot(c, r) => {
                painter.circle_filled(view.to_screen(origin, *c), r * zoom, stroke.color);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_table(
    ui: &Ui,
    painter: &Painter,
    pal: &Palette,
    t: &ErTable,
    geom: &TableGeometry,
    srect: Rect,
    zoom: f32,
    diagram: &ErDiagram,
    selected: bool,
    hot: bool,
    row_highlight: Option<&str>,
) {
    let v = ui.visuals();
    let radius_f = (CORNER_RADIUS * zoom).clamp(0.0, 12.0);
    let radius = radius_f.round() as u8;
    painter.rect_filled(srect, radius, pal.box_fill);
    let header_h = HEADER_HEIGHT * zoom;
    let header = Rect::from_min_size(srect.min, vec2(srect.width(), header_h.min(srect.height())));
    let header_fill = pal.header(v, t.kind, t.status);
    painter.rect_filled(
        header,
        CornerRadius {
            nw: radius,
            ne: radius,
            sw: 0,
            se: 0,
        },
        header_fill,
    );
    if srect.height() > header_h + 1.0 {
        painter.hline(srect.x_range(), header.bottom(), pal.box_stroke);
    }

    // header text
    if let Some(px) = font_px(HEADER_FONT, zoom) {
        let icon_px = px;
        let icon_glyph = if t.kind == ErTableKind::View {
            icon::EYE
        } else {
            icon::TABLE
        };
        let cy = header.center().y;
        let left = srect.left() + PAD_X * zoom;
        paint::text(
            painter,
            pos2(left, cy),
            Align2::LEFT_CENTER,
            icon_glyph,
            icon_px,
            pal.weak,
            false,
            f32::INFINITY,
        );
        let name_x = left + 18.0 * zoom;
        let schema = t.schema.as_deref().filter(|s| !s.is_empty());
        let schema_w = match (schema, font_px(SCHEMA_FONT, zoom)) {
            (Some(s), Some(spx)) => {
                let r = paint::text(
                    painter,
                    pos2(srect.right() - PAD_X * zoom, cy),
                    Align2::RIGHT_CENTER,
                    s,
                    spx,
                    pal.weak,
                    false,
                    (srect.width() * 0.4).max(20.0),
                );
                r.width() + 8.0 * zoom
            }
            _ => 0.0,
        };
        let max_w = (srect.right() - PAD_X * zoom - schema_w - name_x).max(10.0);
        paint::text(
            painter,
            pos2(name_x, cy),
            Align2::LEFT_CENTER,
            &t.name,
            px,
            pal.strong,
            false,
            max_w,
        );
    }

    // comment line
    if let Some(c) = &geom.comment
        && let Some(px) = font_px(COMMENT_FONT, zoom)
    {
        let y = header.bottom() + COMMENT_HEIGHT * zoom * 0.5;
        paint::text(
            painter,
            pos2(srect.left() + PAD_X * zoom, y),
            Align2::LEFT_CENTER,
            c,
            px,
            pal.weak,
            false,
            (srect.width() - 2.0 * PAD_X * zoom).max(10.0),
        );
    }

    // rows
    let row_px = font_px(ROW_FONT, zoom);
    let show_types = diagram.settings.show_types;
    for (i, &ci) in geom.rows.iter().enumerate() {
        let Some(col) = t.columns.get(ci) else {
            continue;
        };
        let top = srect.top() + geom.row_top(i) * zoom;
        let row_rect = Rect::from_min_size(
            pos2(srect.left(), top),
            vec2(srect.width(), ROW_HEIGHT * zoom),
        );
        if !painter.clip_rect().intersects(row_rect) {
            continue;
        }
        if row_highlight == Some(col.name.as_str()) {
            painter.rect_filled(row_rect.shrink2(vec2(2.0, 0.0)), 2.0, pal.row_hover);
        }
        let cy = row_rect.center().y;
        match row_px {
            Some(px) => {
                let ix = srect.left() + PAD_X * zoom;
                if col.pk {
                    paint::text(
                        painter,
                        pos2(ix, cy),
                        Align2::LEFT_CENTER,
                        icon::KEY,
                        px,
                        pal.pk,
                        false,
                        f32::INFINITY,
                    );
                } else if col.fk {
                    paint::text(
                        painter,
                        pos2(ix, cy),
                        Align2::LEFT_CENTER,
                        icon::LINK_SIMPLE,
                        px,
                        pal.fk,
                        false,
                        f32::INFINITY,
                    );
                }
                let name_x = srect.left() + geom.name_x * zoom;
                let type_x = srect.left() + geom.type_x * zoom;
                let has_type = show_types && !col.data_type.is_empty();
                let name_max = if has_type {
                    type_x - name_x - 6.0 * zoom
                } else {
                    srect.right() - PAD_X * zoom - name_x
                };
                let name_color = if col.pk { pal.strong } else { pal.text };
                paint::text(
                    painter,
                    pos2(name_x, cy),
                    Align2::LEFT_CENTER,
                    &col.name,
                    px,
                    name_color,
                    false,
                    name_max.max(8.0),
                );
                if has_type {
                    paint::text(
                        painter,
                        pos2(type_x, cy),
                        Align2::LEFT_CENTER,
                        &col.data_type,
                        px,
                        pal.weak,
                        false,
                        (srect.right() - PAD_X * zoom - type_x).max(8.0),
                    );
                }
            }
            None if zoom >= 0.18 => {
                // skeleton: a bar per row hints at the content
                let w = (text_width(&col.name, ROW_FONT, false) * zoom)
                    .min(srect.width() - 2.0 * (PAD_X + 8.0) * zoom);
                let bar = Rect::from_min_size(
                    pos2(srect.left() + geom.name_x * zoom, cy - 1.5 * zoom.max(0.5)),
                    vec2(w.max(2.0), (3.0 * zoom).max(1.0)),
                );
                let color = if col.pk {
                    pal.pk.gamma_multiply(0.7)
                } else {
                    pal.weak.gamma_multiply(0.45)
                };
                painter.rect_filled(bar, 0.0, color);
            }
            None => {}
        }
    }

    // outline
    let stroke = if selected {
        pal.selection
    } else if hot {
        Stroke::new(1.5, pal.highlight)
    } else if let Some(c) = status_stroke(t.status) {
        Stroke::new(1.5, c)
    } else {
        pal.box_stroke
    };
    painter.rect_stroke(srect, radius, stroke, StrokeKind::Inside);
}

#[allow(clippy::too_many_arguments)]
fn minimap(
    ui: &Ui,
    painter: &Painter,
    canvas: Rect,
    base_id: Id,
    scene: &ErScene,
    diagram: &ErDiagram,
    state: &mut ErViewState,
    pal: &Palette,
) {
    let Some(bounds) = scene.bounds() else { return };
    let bounds = bounds.expand(20.0);
    let max = vec2(200.0, 140.0).min(canvas.size() * 0.3);
    if max.x < 40.0 || max.y < 30.0 {
        return;
    }
    let scale = (max.x / bounds.width()).min(max.y / bounds.height());
    let size = bounds.size() * scale;
    let inner = Rect::from_min_size(canvas.max - size - vec2(12.0, 12.0), size);
    let frame = inner.expand(4.0);
    state.minimap_rect = Some(frame);
    let to_map = |p: Pos2| inner.min + (p - bounds.min) * scale;
    painter.rect_filled(frame, 4.0, pal.box_fill.gamma_multiply(0.92));
    painter.rect_stroke(frame, 4.0, pal.box_stroke, StrokeKind::Inside);
    let v = ui.visuals();
    for st in &scene.tables {
        let Some(t) = diagram.model.tables.get(st.table) else {
            continue;
        };
        let r = Rect::from_min_max(to_map(st.rect.min), to_map(st.rect.max));
        let mut fill = pal.header(v, t.kind, t.status);
        if state.selection.contains(&t.key) {
            fill = pal.highlight;
        }
        painter.rect_filled(r, 0.0, fill);
    }
    let view = state.view();
    let visible = view.rect_to_world(canvas.min, canvas);
    let vr = Rect::from_min_max(to_map(visible.min), to_map(visible.max)).intersect(frame);
    painter.rect_stroke(vr, 0.0, Stroke::new(1.5, pal.highlight), StrokeKind::Inside);

    let resp = ui.interact(frame, base_id.with("minimap"), Sense::click_and_drag());
    if (resp.clicked() || resp.dragged())
        && let Some(p) = resp.interact_pointer_pos()
    {
        let world = bounds.min + (p - inner.min) / scale;
        state.pan = canvas.size() * 0.5 - world.to_vec2() * state.zoom;
    }
    resp.on_hover_text(tr("Overview: click or drag to move the view"));
}

fn hover_tooltip(ui: &mut Ui, diagram: &ErDiagram, hover: &ErHover) {
    match hover {
        ErHover::Table(k) | ErHover::Column { table: k, .. } => {
            let Some(t) = diagram.model.table(k) else {
                return;
            };
            ui.label(egui::RichText::new(t.key.as_str()).strong());
            let kind = if t.kind == ErTableKind::View {
                tr("View")
            } else {
                tr("Table")
            };
            ui.weak(trf("{} · {} columns", &[&kind, &t.columns.len()]));
            if let Some(s) = t.status {
                ui.label(tr(s.label()));
            }
            if let Some(c) = &t.comment {
                ui.label(c.as_str());
            }
            if let ErHover::Column { column, .. } = hover
                && let Some(c) = t.column(column)
            {
                ui.separator();
                let mut flags = Vec::new();
                if c.pk {
                    flags.push(tr("primary key"));
                }
                if c.fk {
                    flags.push(tr("foreign key"));
                }
                if c.unique && !c.pk {
                    flags.push(tr("unique"));
                }
                flags.push(if c.nullable {
                    tr("nullable")
                } else {
                    tr("not null")
                });
                ui.label(egui::RichText::new(format!("{} {}", c.name, c.data_type)).monospace());
                ui.weak(flags.join(", "));
                for r in diagram
                    .model
                    .relations
                    .iter()
                    .filter(|r| r.from_table == t.key && r.from_columns.contains(column))
                {
                    ui.weak(trf(
                        "references {}({})",
                        &[&r.to_table, &r.to_columns.join(", ")],
                    ));
                }
            }
        }
        ErHover::Relation(i) => {
            let Some(r) = diagram.model.relations.get(*i) else {
                return;
            };
            ui.label(egui::RichText::new(r.name.as_str()).strong());
            ui.label(format!("{}({})", r.from_table, r.from_columns.join(", ")));
            ui.label(format!("→ {}({})", r.to_table, r.to_columns.join(", ")));
            let mut notes = Vec::new();
            if r.identifying {
                notes.push(tr("identifying"));
            }
            notes.push(if r.optional {
                tr("optional")
            } else {
                tr("mandatory")
            });
            ui.weak(notes.join(", "));
        }
        ErHover::Note(_) => {
            ui.weak(tr("Note – double click to edit"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::er::model::{ErColumn, ErModel, ErRelation, ErTable, ErTableKind};
    use crate::test_harness::Harness;

    fn diagram() -> ErDiagram {
        let mut m = ErModel::default();
        m.tables.push(
            ErTable::new(None, "a", ErTableKind::Table)
                .with_column(ErColumn::new("id", "int").primary_key()),
        );
        m.tables.push(
            ErTable::new(None, "b", ErTableKind::Table)
                .with_column(ErColumn::new("id", "int").primary_key())
                .with_column(ErColumn::new("a_id", "int").foreign_key()),
        );
        m.tables.push(
            ErTable::new(None, "c", ErTableKind::View).with_column(ErColumn::new("x", "text")),
        );
        m.relations.push(ErRelation {
            name: "b_a".into(),
            from_table: "b".into(),
            from_columns: vec!["a_id".into()],
            to_table: "a".into(),
            to_columns: vec!["id".into()],
            identifying: false,
            optional: true,
        });
        ErDiagram::new("t", m)
    }

    /// Screen rectangle of a table.
    fn screen_rect(d: &ErDiagram, s: &ErViewState, key: &str) -> Rect {
        let r = d.table_rect(key).expect("table");
        s.view().rect_to_screen(s.last_canvas.min, r)
    }

    #[test]
    fn interactions_produce_events() {
        let mut h = Harness::new(1200.0, 800.0);
        let mut d = diagram();
        let mut s = ErViewState::default();
        let mut events: Vec<ErEvent> = Vec::new();
        let mut ui_fn = |ui: &mut Ui| events.extend(show_er(ui, &mut d, &mut s).events);
        h.idle(3, &mut ui_fn);
        drop(ui_fn);
        assert!(!s.fit_pending, "fit happened once the size was stable");
        assert!(s.zoom > 0.1 && s.zoom <= 1.0);

        // click selects
        let a = screen_rect(&d, &s, "a");
        let mut ui_fn = |ui: &mut Ui| events.extend(show_er(ui, &mut d, &mut s).events);
        h.click(a.center(), &mut ui_fn);
        drop(ui_fn);
        assert!(
            events.contains(&ErEvent::SelectionChanged(vec!["a".into()])),
            "{events:?}"
        );
        assert_eq!(s.selection, BTreeSet::from(["a".to_string()]));

        // dragging the header moves the table
        let before = d.positions["b"];
        let b = screen_rect(&d, &s, "b");
        let grab = pos2(b.center().x, b.top() + 6.0);
        events.clear();
        let mut ui_fn = |ui: &mut Ui| events.extend(show_er(ui, &mut d, &mut s).events);
        h.drag(grab, grab + vec2(120.0, 60.0), &mut ui_fn);
        drop(ui_fn);
        assert!(
            events.contains(&ErEvent::TableMoved(vec!["b".into()])),
            "{events:?}"
        );
        let moved = d.positions["b"] - before;
        assert!(
            moved.x * s.zoom > 80.0 && moved.y * s.zoom > 30.0,
            "moved {moved:?} at zoom {}",
            s.zoom
        );

        // double click opens
        events.clear();
        let c = screen_rect(&d, &s, "c");
        let mut ui_fn = |ui: &mut Ui| events.extend(show_er(ui, &mut d, &mut s).events);
        h.double_click(c.center(), &mut ui_fn);
        drop(ui_fn);
        assert!(
            events.contains(&ErEvent::OpenTable("c".into())),
            "{events:?}"
        );

        // edit mode: dragging a column onto another table proposes a foreign key
        s.edit_mode = true;
        events.clear();
        let b = screen_rect(&d, &s, "b");
        let geom = d.geometry("b").expect("geometry");
        let a_id_row = pos2(
            b.center().x,
            b.top() + (geom.row_top(1) + ROW_HEIGHT * 0.5) * s.zoom,
        );
        let a = screen_rect(&d, &s, "a");
        let target = pos2(a.center().x, a.top() + 6.0);
        let mut ui_fn = |ui: &mut Ui| events.extend(show_er(ui, &mut d, &mut s).events);
        h.drag(a_id_row, target, &mut ui_fn);
        drop(ui_fn);
        assert!(
            events.contains(&ErEvent::CreateRelation {
                from_table: "b".into(),
                from_columns: vec!["a_id".into()],
                to_table: "a".into(),
                to_columns: vec!["id".into()],
            }),
            "{events:?}"
        );

        // the delete key in edit mode asks to drop the selection
        events.clear();
        let a = screen_rect(&d, &s, "a");
        let mut ui_fn = |ui: &mut Ui| events.extend(show_er(ui, &mut d, &mut s).events);
        h.click(a.center(), &mut ui_fn);
        h.key(Key::Delete, &mut ui_fn);
        drop(ui_fn);
        assert!(
            events.contains(&ErEvent::DeleteTables(vec!["a".into()])),
            "{events:?}"
        );

        // right click opens the context menu without panicking
        let mut ui_fn = |ui: &mut Ui| events.extend(show_er(ui, &mut d, &mut s).events);
        h.click_with(a.center(), PointerButton::Secondary, &mut ui_fn);
        h.idle(2, &mut ui_fn);
        drop(ui_fn);
        assert_eq!(s.context, Some(ContextTarget::Table("a".into())));
    }

    #[test]
    fn pan_zoom_and_rubber_band() {
        let mut h = Harness::new(1000.0, 700.0);
        let mut d = diagram();
        let mut s = ErViewState::default();
        let mut ui_fn = |ui: &mut Ui| {
            show_er(ui, &mut d, &mut s);
        };
        h.idle(3, &mut ui_fn);
        drop(ui_fn);
        // dragging the empty background pans
        let pan_before = s.pan;
        let empty = s.last_canvas.max - vec2(30.0, 200.0);
        let mut ui_fn = |ui: &mut Ui| {
            show_er(ui, &mut d, &mut s);
        };
        h.drag(empty, empty - vec2(100.0, 0.0), &mut ui_fn);
        drop(ui_fn);
        assert!(
            (s.pan.x - pan_before.x) < -60.0,
            "{pan_before:?} -> {:?}",
            s.pan
        );
        // scrolling pans too
        let pan_before = s.pan;
        let mut ui_fn = |ui: &mut Ui| {
            show_er(ui, &mut d, &mut s);
        };
        h.scroll(empty, vec2(0.0, -50.0), &mut ui_fn);
        drop(ui_fn);
        assert!(s.pan.y < pan_before.y, "{pan_before:?} -> {:?}", s.pan);
        // shift + drag selects with a rubber band covering everything
        s.fit_pending = true;
        let mut ui_fn = |ui: &mut Ui| {
            show_er(ui, &mut d, &mut s);
        };
        h.idle(2, &mut ui_fn);
        drop(ui_fn);
        let r = s.last_canvas;
        h.set_modifiers(Modifiers::SHIFT);
        let mut ui_fn = |ui: &mut Ui| {
            show_er(ui, &mut d, &mut s);
        };
        h.drag(r.min + vec2(3.0, 3.0), r.max - vec2(3.0, 3.0), &mut ui_fn);
        drop(ui_fn);
        h.set_modifiers(Modifiers::NONE);
        assert_eq!(s.selection.len(), 3, "{:?}", s.selection);
        // hovering a relation reports it
        let scene = ErScene::build(&d);
        let route = &scene.relations[0].route;
        let probe = route.points[0] + route.start_dir * 6.0;
        let sp = s.view().to_screen(s.last_canvas.min, probe);
        let mut hovered = None;
        let mut ui_fn = |ui: &mut Ui| {
            hovered = show_er(ui, &mut d, &mut s).hovered;
        };
        h.hover(sp, &mut ui_fn);
        drop(ui_fn);
        assert_eq!(hovered, Some(ErHover::Relation(0)));
    }

    #[test]
    fn large_diagram_renders_at_every_zoom() {
        let model = crate::er::layout::tests::generated_model(120, 3);
        let mut d = ErDiagram::new("big", model);
        d.settings.show_comments = true;
        let mut h = Harness::new(1400.0, 900.0);
        for zoom in [0.05, 0.12, 0.3, 0.5, 1.0, 2.5] {
            let mut s = ErViewState {
                fit_pending: false,
                zoom,
                ..Default::default()
            };
            let mut ui_fn = |ui: &mut Ui| {
                show_er(ui, &mut d, &mut s);
            };
            h.idle(2, &mut ui_fn);
            h.hover(pos2(700.0, 450.0), &mut ui_fn);
        }
        // simple notation and an empty diagram also render
        d.settings.notation = Notation::Simple;
        let mut s = ErViewState::default();
        let mut empty = ErDiagram::default();
        let mut empty_state = ErViewState::default();
        let mut ui_fn = |ui: &mut Ui| {
            show_er(ui, &mut d, &mut s);
            ui.push_id("second", |ui| show_er(ui, &mut empty, &mut empty_state));
        };
        h.idle(3, &mut ui_fn);
    }
}
