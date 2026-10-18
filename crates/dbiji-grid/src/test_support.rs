//! Headless egui harness shared by the UI tests.

use crate::grid::{DataGrid, DataGridState, GridAction, GridOptions};
use crate::model::{ResultModel, determine_editability};
use dbiji_core::{ColumnMeta, ColumnSource, DataKind, Value};
use egui::{Event, Key, Modifiers, PointerButton, Pos2, RawInput, Rect, vec2};

/// Runs frames of arbitrary UI code.
pub(crate) struct Ui {
    pub ctx: egui::Context,
    pub time: f64,
    pub size: egui::Vec2,
    /// Repaint delay requested by the last frame (`Duration::MAX` = none).
    pub repaint_delay: std::time::Duration,
    modifiers: Modifiers,
}

impl Ui {
    pub fn new() -> Self {
        Ui { ctx: egui::Context::default(), time: 0.0, size: vec2(900.0, 600.0), repaint_delay: std::time::Duration::MAX, modifiers: Modifiers::NONE }
    }

    /// One frame with `events`; returns the platform output.
    pub fn run(&mut self, events: Vec<Event>, modifiers: Modifiers, mut f: impl FnMut(&mut egui::Ui)) -> egui::PlatformOutput {
        self.time += 1.0 / 60.0;
        // Like a real integration: only report modifier changes (any event triggers a repaint).
        let mut all = Vec::new();
        if modifiers != self.modifiers {
            all.push(Event::ModifiersChanged(modifiers));
            self.modifiers = modifiers;
        }
        all.extend(events);
        let input = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, self.size)), time: Some(self.time), events: all, focused: true, ..Default::default() };
        let mut out = self.ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| f(ui));
        });
        out.textures_delta.clear();
        self.repaint_delay = out.viewport_output.get(&egui::ViewportId::ROOT).map_or(std::time::Duration::MAX, |v| v.repaint_delay);
        out.platform_output
    }
}

pub(crate) fn key(key: Key, modifiers: Modifiers) -> Event {
    Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
}

pub(crate) fn press(p: Pos2, button: PointerButton, modifiers: Modifiers) -> Event {
    Event::PointerButton { pos: p, button, pressed: true, modifiers }
}

pub(crate) fn release(p: Pos2, button: PointerButton, modifiers: Modifiers) -> Event {
    Event::PointerButton { pos: p, button, pressed: false, modifiers }
}

fn src(col: &str) -> ColumnSource {
    ColumnSource { database: None, schema: Some("app".into()), table: "t".into(), column: col.into() }
}

/// Editable model: id (key), name, flag (bool), category (dictionary, FK).
pub(crate) fn editable_model(n: usize) -> ResultModel {
    let cols = vec![
        ColumnMeta::new("id", "int", DataKind::Int).with_source(src("id")),
        ColumnMeta::new("name", "text", DataKind::Text).with_source(src("name")),
        ColumnMeta::new("flag", "bool", DataKind::Bool).with_source(src("flag")),
        ColumnMeta::new("category", "int", DataKind::Int).with_source(src("category")),
    ];
    let rows = (0..n).map(|i| vec![Value::Int(i as i64), Value::text(format!("name {i}")), Value::Bool(i % 2 == 0), Value::Int((i % 3) as i64 + 1)]).collect();
    let mut m = ResultModel::from_rows(cols, rows);
    let e = determine_editability(m.columns(), |_| Some(vec!["id".into()]), false);
    m.set_editability(e);
    m
}

/// A grid with its model and state.
pub(crate) struct GridHarness {
    pub ui: Ui,
    pub model: ResultModel,
    pub state: DataGridState,
    pub options: GridOptions,
    pub actions: Vec<GridAction>,
    pub output: egui::PlatformOutput,
}

impl GridHarness {
    pub fn new(model: ResultModel) -> Self {
        let state = DataGridState::for_model(&model);
        let mut h = GridHarness { ui: Ui::new(), model, state, options: GridOptions::default(), actions: Vec::new(), output: Default::default() };
        h.frame(vec![]);
        h.frame(vec![]);
        h
    }

    pub fn frame_mods(&mut self, events: Vec<Event>, modifiers: Modifiers) {
        let mut actions = Vec::new();
        let (model, state, options) = (&mut self.model, &mut self.state, &self.options);
        self.output = self.ui.run(events, modifiers, |ui| {
            actions = DataGrid::new(model, state, options).show(ui).actions;
        });
        self.actions = actions;
    }

    pub fn frame(&mut self, events: Vec<Event>) {
        self.frame_mods(events, Modifiers::NONE);
    }

    pub fn key(&mut self, k: Key, modifiers: Modifiers) {
        self.frame_mods(vec![key(k, modifiers)], modifiers);
    }

    /// (body, header, row numbers, row height) of the last frame.
    pub fn geometry(&self) -> (Rect, Rect, Rect, f32) {
        self.state.rt.last_geometry.unwrap_or((Rect::NOTHING, Rect::NOTHING, Rect::NOTHING, 20.0))
    }

    /// Centre of a cell given by display row and visible column position.
    pub fn cell_center(&self, row: usize, pos: usize) -> Pos2 {
        let (body, _, _, row_h) = self.geometry();
        let (sx, sy) = self.state.scroll_offset();
        let cols = self.state.visible_columns();
        let x0: f32 = cols[..pos].iter().map(|&c| self.state.column_width(c)).sum();
        let w = self.state.column_width(cols[pos]);
        let x = body.left() + x0 - sx + w / 2.0;
        let y = body.top() + (row as f64 * row_h as f64 - sy) as f32 + row_h / 2.0;
        Pos2::new(x, y)
    }

    /// Right border of a header cell.
    pub fn header_border(&self, pos: usize) -> Pos2 {
        let (_, header, _, _) = self.geometry();
        let (sx, _) = self.state.scroll_offset();
        let cols = self.state.visible_columns();
        let x: f32 = cols[..=pos].iter().map(|&c| self.state.column_width(c)).sum();
        Pos2::new(header.left() + x - sx, header.center().y)
    }

    pub fn click_at(&mut self, p: Pos2, button: PointerButton, modifiers: Modifiers) {
        self.frame_mods(vec![Event::PointerMoved(p)], modifiers);
        self.frame_mods(vec![press(p, button, modifiers)], modifiers);
        self.frame_mods(vec![release(p, button, modifiers)], modifiers);
    }

    pub fn drag(&mut self, from: Pos2, to: Pos2) {
        self.frame(vec![Event::PointerMoved(from)]);
        self.frame(vec![press(from, PointerButton::Primary, Modifiers::NONE)]);
        let steps = 6;
        for i in 1..=steps {
            let t = i as f32 / steps as f32;
            self.frame(vec![Event::PointerMoved(from + (to - from) * t)]);
        }
        self.frame(vec![release(to, PointerButton::Primary, Modifiers::NONE)]);
        self.frame(vec![]);
    }

    pub fn copied(&self) -> Option<String> {
        self.output.commands.iter().find_map(|c| match c {
            egui::OutputCommand::CopyText(t) => Some(t.clone()),
            _ => None,
        })
    }

    /// Run frames until the background view job finished.
    pub fn wait_view_job(&mut self) {
        for _ in 0..500 {
            if !self.model.is_view_job_running() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
            self.frame(vec![]);
        }
        self.frame(vec![]);
    }
}
