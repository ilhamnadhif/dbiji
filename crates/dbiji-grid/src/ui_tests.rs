//! Headless tests of the composite viewer, toolbar, views and panels.

use crate::grid::{GridAction, GridOptions, PanelKind, ResultViewKind};
use crate::model::ResultModel;
use crate::panels::{GroupingPanel, GroupingPanelState, ReferencesPanel, ReferencesPanelState, Relation, RelationDirection};
use crate::result_view::{ResultViewState, ResultViewer};
use crate::test_support::{Ui, editable_model, press, release};
use dbiji_core::{ColumnMeta, DataKind, ObjectRef, Value};
use egui::{Event, Modifiers, PointerButton, Pos2};

struct Viewer {
    ui: Ui,
    model: ResultModel,
    state: ResultViewState,
    options: GridOptions,
    actions: Vec<GridAction>,
    output: egui::PlatformOutput,
}

impl Viewer {
    fn new(model: ResultModel) -> Self {
        let state = ResultViewState::new(&model);
        let mut v = Viewer { ui: Ui::new(), model, state, options: GridOptions::default(), actions: Vec::new(), output: Default::default() };
        v.ui.size = egui::vec2(1200.0, 700.0);
        v.ui.ctx.enable_accesskit();
        v.frame(vec![]);
        v
    }

    fn frame(&mut self, events: Vec<Event>) {
        self.frame_with(events, Modifiers::NONE);
    }

    /// A frame with `modifiers` held.
    fn frame_with(&mut self, events: Vec<Event>, modifiers: Modifiers) {
        let mut actions = Vec::new();
        let (model, state, options) = (&mut self.model, &mut self.state, &self.options);
        self.output = self.ui.run(events, modifiers, |ui| {
            actions = ResultViewer::new(model, state, options).show(ui).actions;
        });
        self.actions = actions;
    }

    /// Centre of the first accessibility node whose label contains `text`.
    fn find(&self, text: &str) -> Option<Pos2> {
        let update = self.output.accesskit_update.as_ref()?;
        update.nodes.iter().find_map(|(_, n)| {
            let label = n.label().or(n.value())?;
            if !label.contains(text) {
                return None;
            }
            let b = n.bounds()?;
            Some(Pos2::new(((b.x0 + b.x1) / 2.0) as f32, ((b.y0 + b.y1) / 2.0) as f32))
        })
    }

    /// Centre of the first text input holding `text`.
    fn find_input(&self, text: &str) -> Option<Pos2> {
        let update = self.output.accesskit_update.as_ref()?;
        update.nodes.iter().find_map(|(_, n)| {
            if !matches!(n.role(), egui::accesskit::Role::TextInput | egui::accesskit::Role::MultilineTextInput) || !n.value().or(n.label()).is_some_and(|l| l.contains(text)) {
                return None;
            }
            let b = n.bounds()?;
            Some(Pos2::new(((b.x0 + b.x1) / 2.0) as f32, ((b.y0 + b.y1) / 2.0) as f32))
        })
    }

    fn click(&mut self, p: Pos2) {
        self.frame(vec![Event::PointerMoved(p)]);
        self.frame(vec![press(p, PointerButton::Primary, Modifiers::NONE)]);
        self.frame(vec![release(p, PointerButton::Primary, Modifiers::NONE)]);
    }

    fn wait_jobs(&mut self) {
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

#[test]
fn state_persists_and_types_are_send() {
    fn assert_send<T: Send>() {}
    assert_send::<ResultModel>();
    assert_send::<crate::grid::DataGridState>();
    assert_send::<crate::view::ClientFilter>();
    let mut v = Viewer::new(editable_model(5));
    v.state.grid.set_column_width(1, 333.0);
    v.state.grid.set_column_visible(2, false);
    v.state.toolbar.filter_text = "id > 1".into();
    v.state.toolbar.auto_refresh_secs = Some(30);
    v.state.open_panel(PanelKind::Calc);
    let json = serde_json::to_string(&v.state).unwrap();
    let back: ResultViewState = serde_json::from_str(&json).unwrap();
    assert_eq!(back.grid.column_width(1), 333.0);
    assert!(!back.grid.columns[2].visible);
    assert_eq!(back.toolbar.filter_text, "id > 1");
    assert_eq!(back.toolbar.auto_refresh_secs, Some(30));
    assert!(back.toolbar.is_panel_open(PanelKind::Calc));
}

#[test]
fn idle_frames_request_no_repaint() {
    let mut v = Viewer::new(editable_model(500));
    v.state.grid.select_cell(2, 1);
    v.state.open_panel(PanelKind::Calc);
    v.state.open_panel(PanelKind::Metadata);
    v.state.open_panel(PanelKind::Value);
    for kind in [ResultViewKind::Grid, ResultViewKind::Text, ResultViewKind::Record, ResultViewKind::Json] {
        v.state.toolbar.view = kind;
        // Let one-off animations (scroll bars fading in) finish.
        for _ in 0..40 {
            v.frame(vec![]);
        }
        assert_eq!(v.ui.repaint_delay, std::time::Duration::MAX, "{kind:?} keeps repainting while idle");
    }
    // A transient status message schedules exactly one timed repaint, not a continuous one.
    v.state.toolbar.view = ResultViewKind::Grid;
    v.state.grid.show_message("hello", false);
    v.frame(vec![]);
    assert!(v.ui.repaint_delay > std::time::Duration::from_millis(500));
}

#[test]
fn every_view_and_panel_renders() {
    let mut v = Viewer::new(editable_model(200));
    v.state.grid.select_cell(3, 1);
    for p in [PanelKind::Value, PanelKind::Metadata, PanelKind::Grouping, PanelKind::Calc, PanelKind::References] {
        v.state.open_panel(p);
        v.frame(vec![]);
        v.frame(vec![]);
    }
    for kind in [ResultViewKind::Text, ResultViewKind::Record, ResultViewKind::Json, ResultViewKind::Xml, ResultViewKind::Grid] {
        v.state.toolbar.view = kind;
        v.frame(vec![]);
        v.frame(vec![]);
    }
    // The text view caches its rendering.
    v.state.toolbar.view = ResultViewKind::Text;
    v.frame(vec![]);
    assert!(v.find("name 3").is_some(), "text view shows rows");
}

#[test]
fn client_filter_from_filter_bar() {
    let mut v = Viewer::new(editable_model(100));
    v.state.toolbar.filter_text = "id >= 90 and flag".into();
    v.state.toolbar.order_text = "id desc".into();
    // Press Enter in the filter field: simulate by clicking the apply button (labelled ▶ icon).
    let apply = v.find(egui_phosphor::regular::PLAY).expect("apply button");
    v.click(apply);
    v.wait_jobs();
    assert_eq!(v.model.row_count(), 5);
    assert_eq!(v.model.cell(0, 0), Some(Value::Int(98)));
    assert_eq!(v.state.toolbar.filter_history.first().map(String::as_str), Some("id >= 90 and flag"));
    // A broken filter reports an error with its position and changes nothing.
    v.state.toolbar.filter_text = "id >> 3".into();
    v.click(apply);
    assert!(v.state.toolbar.filter_error().is_some_and(|e| e.contains("position")));
    assert_eq!(v.model.row_count(), 5);
}

#[test]
fn server_mode_filter_bar_emits_query_change() {
    let mut v = Viewer::new(editable_model(10));
    v.state.grid.mode = crate::grid::DataMode::Server;
    v.state.toolbar.filter_text = "status = 'open'".into();
    let apply = v.find(egui_phosphor::regular::PLAY).expect("apply button");
    v.frame(vec![Event::PointerMoved(apply)]);
    v.frame(vec![press(apply, PointerButton::Primary, Modifiers::NONE)]);
    v.frame(vec![release(apply, PointerButton::Primary, Modifiers::NONE)]);
    let changed = v.actions.iter().any(|a| matches!(a, GridAction::ServerQueryChanged { where_clause: Some(w), .. } if w == "status = 'open'"));
    assert!(changed, "{:?}", v.actions);
}

#[test]
fn value_panel_edits_active_cell() {
    let mut v = Viewer::new(editable_model(10));
    v.state.grid.select_cell(2, 1);
    v.state.open_panel(PanelKind::Value);
    v.frame(vec![]);
    v.frame(vec![]);
    // Focus the editor and type.
    let editor = v.find_input("name 2").expect("value editor");
    v.click(editor);
    v.frame(vec![crate::test_support::key(egui::Key::End, Modifiers::NONE)]);
    v.frame(vec![Event::Text("!".into())]);
    let apply = v.find("Apply").expect("apply button");
    v.click(apply);
    assert_eq!(v.model.cell(2, 1), Some(Value::text("name 2!")));
}

#[test]
fn auto_refresh_emits_refresh() {
    let mut v = Viewer::new(editable_model(5));
    v.state.toolbar.auto_refresh_secs = Some(5);
    v.frame(vec![]);
    assert!(!v.actions.contains(&GridAction::Refresh));
    v.ui.time += 6.0;
    v.frame(vec![]);
    assert!(v.actions.contains(&GridAction::Refresh));
    // Paused while there are unsaved changes.
    v.model.set_cell(0, 1, "x").unwrap();
    v.ui.time += 6.0;
    v.frame(vec![]);
    assert!(!v.actions.contains(&GridAction::Refresh));
}

#[test]
fn open_value_panel_action_opens_panel() {
    let mut v = Viewer::new(editable_model(5));
    v.model.set_cell(1, 1, "line one\nline two").unwrap();
    v.state.grid.select_cell(1, 1);
    v.frame(vec![]);
    // Enter on a multi-line value opens the value panel instead of the inline editor.
    let cell = v.find("Data grid").expect("grid");
    v.click(cell);
    v.state.grid.select_cell(1, 1);
    v.frame(vec![]);
    v.frame(vec![crate::test_support::key(egui::Key::Enter, Modifiers::NONE)]);
    v.frame(vec![]);
    assert!(v.state.toolbar.is_panel_open(PanelKind::Value));
    assert_eq!(v.state.active_panel, Some(PanelKind::Value));
}

#[test]
fn a_refused_edit_pops_up_once() {
    let mut m = editable_model(5);
    m.set_editability(Err(crate::model::ReadOnlyReason::NoKey));
    let mut v = Viewer::new(m);
    let cell = v.find("Data grid").expect("grid");
    v.click(cell);
    v.state.grid.select_cell(1, 1);
    v.frame(vec![]);
    v.frame(vec![crate::test_support::key(egui::Key::Enter, Modifiers::NONE)]);
    // The pop-up opens, then settles in the middle (sized on its first frames).
    for _ in 0..4 {
        v.frame(vec![]);
    }
    assert!(!v.state.grid.is_editing());
    assert!(v.state.grid.rt.message.is_none(), "no overlay on the grid");
    // Once, in full, in a pop-up with its remedy.
    let count = |v: &Viewer, text: &str| {
        v.output
            .accesskit_update
            .as_ref()
            .map_or(0, |u| u.nodes.iter().filter(|(_, n)| n.role() != egui::accesskit::Role::TextRun && n.label().or(n.value()).is_some_and(|l| l.contains(text))).count())
    };
    assert_eq!(count(&v, "Define a virtual key (columns whose values identify a row)"), 1);
    assert_eq!(v.state.toolbar.error_popup.as_deref().map(|m| m.contains("no primary or unique key")), Some(true));
    assert!(v.state.toolbar.notice.is_none(), "not in the status bar");
    let button = v.find("Define virtual key").expect("Define virtual key… in the pop-up");
    v.click(button);
    assert!(v.actions.iter().any(|a| matches!(a, GridAction::DefineVirtualKey { .. })));
    v.frame(vec![]);
    assert_eq!(count(&v, "Define a virtual key (columns whose values identify a row)"), 0, "taken care of");
}

#[test]
fn grouping_panel_groups_in_background() {
    let mut ui = Ui::new();
    let mut model = editable_model(300);
    let mut state = GroupingPanelState::default();
    state.spec.columns = vec![3];
    state.spec.measure = Some(0);
    let mut action = None;
    for _ in 0..400 {
        ui.run(vec![], Modifiers::NONE, |ui| {
            action = GroupingPanel::new(&mut model, &mut state).show(ui);
        });
        if !state.is_running() && state.result().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(action.is_none());
    let result = state.result().expect("grouping finished");
    assert_eq!(result.groups.len(), 3);
    assert!(result.groups.iter().all(|g| g.count == 100));
    assert!(result.groups.iter().all(|g| g.sum.is_some()));
}

#[test]
fn references_panel_requests_and_shows_related_rows() {
    let mut ui = Ui::new();
    let model = editable_model(5);
    let mut grid = crate::grid::DataGridState::for_model(&model);
    grid.select_cell(1, 0);
    let mut state = ReferencesPanelState::default();
    state.set_relations(vec![Relation {
        name: Some("fk".into()),
        direction: RelationDirection::References,
        table: ObjectRef::table(Some("app".into()), "categories"),
        columns: vec!["id".into()],
        key_columns: vec![3],
    }]);
    let mut actions = Vec::new();
    ui.run(vec![], Modifiers::NONE, |ui| actions = ReferencesPanel::new(&model, &grid, &mut state).show(ui));
    assert!(matches!(&actions[..], [GridAction::RequestReferences { relation: 0, values }] if values == &vec![Value::Int(2)]));
    // Not requested again for the same key.
    ui.run(vec![], Modifiers::NONE, |ui| actions = ReferencesPanel::new(&model, &grid, &mut state).show(ui));
    assert!(actions.is_empty());
    state.set_related_rows(
        0,
        vec![Value::Int(2)],
        vec![ColumnMeta::new("id", "int", DataKind::Int), ColumnMeta::new("name", "text", DataKind::Text)],
        vec![vec![Value::Int(2), Value::text("Music")]],
    );
    ui.run(vec![], Modifiers::NONE, |ui| actions = ReferencesPanel::new(&model, &grid, &mut state).show(ui));
    assert!(actions.is_empty());
}

#[test]
fn calc_values_copy() {
    let mut v = Viewer::new(editable_model(10));
    v.state.grid.select_cell(2, 0);
    v.state.open_panel(PanelKind::Calc);
    v.frame(vec![]);
    v.frame(vec![]);
    let copied = |v: &Viewer| {
        v.output.commands.iter().find_map(|c| match c {
            egui::OutputCommand::CopyText(t) => Some(t.clone()),
            _ => None,
        })
    };
    // A click selects the row, ⌘C copies its value.
    let count = v.find("Count: 1").expect("count row");
    v.click(count);
    v.frame(vec![Event::Copy]);
    assert_eq!(copied(&v).as_deref(), Some("1"));
    // The copy button at the end of a hovered row.
    let bounds = v.output.accesskit_update.as_ref().and_then(|u| u.nodes.iter().find_map(|(_, n)| n.label().or(n.value()).filter(|l| l.starts_with("Distinct")).and(n.bounds())));
    let button = bounds.map(|b| egui::pos2(b.x1 as f32 - 14.0, ((b.y0 + b.y1) / 2.0) as f32)).expect("distinct row");
    v.click(button);
    assert_eq!(copied(&v).as_deref(), Some("1"));
}

#[test]
fn record_view_picks_several_values_and_copies_them() {
    let mut v = Viewer::new(editable_model(5));
    v.state.grid.select_cell(1, 0);
    v.state.grid.selection_mut().extend_to(crate::grid::CellPos::new(2, 0));
    v.state.toolbar.view = ResultViewKind::Record;
    v.frame(vec![]);
    v.frame(vec![]);
    let lead = |v: &Viewer, name: &str| -> Pos2 {
        let update = v.output.accesskit_update.as_ref().expect("accesskit");
        update
            .nodes
            .iter()
            .find_map(|(_, n)| {
                let b = n.bounds()?;
                (n.label().or(n.value()) == Some(name)).then(|| Pos2::new(((b.x0 + b.x1) / 2.0) as f32, ((b.y0 + b.y1) / 2.0) as f32))
            })
            .unwrap_or_else(|| panic!("no column name {name}"))
    };
    // A column name picks it in both rows; ⌘-click on another adds it.
    let name = lead(&v, "name");
    v.click(name);
    let flag = lead(&v, "flag");
    v.frame(vec![Event::PointerMoved(flag)]);
    let cmd = Modifiers::COMMAND;
    v.frame_with(vec![press(flag, PointerButton::Primary, cmd)], cmd);
    v.frame_with(vec![release(flag, PointerButton::Primary, cmd)], cmd);
    v.frame(vec![Event::Copy]);
    let copied = v.output.commands.iter().find_map(|c| match c {
        egui::OutputCommand::CopyText(t) => Some(t.clone()),
        _ => None,
    });
    assert_eq!(copied.as_deref(), Some("name 1\tname 2\nfalse\ttrue"));
    // The rows shown side by side stay selected in the grid.
    assert_eq!(v.state.grid.selected_rows(5, 10), vec![1, 2]);
}
