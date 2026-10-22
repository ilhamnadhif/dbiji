//! Headless egui tests of the grid widget.

use super::*;
use crate::model::{ForeignKeyLink, ReadOnlyReason, RowState};
use crate::test_support::{GridHarness, editable_model};
use dbiji_core::Value;
use egui::{Event, Key, Modifiers, PointerButton};

#[test]
fn renders_and_navigates_with_keyboard() {
    let mut h = GridHarness::new(editable_model(100));
    let p = h.cell_center(0, 0);
    h.click_at(p, PointerButton::Primary, Modifiers::NONE);
    assert_eq!(h.state.active_cell(), Some((0, 0)), "clicked cell is active");
    h.key(Key::ArrowDown, Modifiers::NONE);
    assert_eq!(h.state.active_cell(), Some((1, 0)));
    h.key(Key::ArrowRight, Modifiers::NONE);
    h.key(Key::End, Modifiers::COMMAND);
    assert_eq!(h.state.active_cell(), Some((99, 3)));
    assert!(h.state.scroll_offset().1 > 0.0, "scrolled to the last row");
    h.key(Key::Home, Modifiers::COMMAND);
    assert_eq!(h.state.active_cell(), Some((0, 0)));
    h.key(Key::ArrowDown, Modifiers::SHIFT);
    h.key(Key::ArrowRight, Modifiers::SHIFT);
    assert_eq!(h.state.selection().cell_count(100, 4), 4);
    h.key(Key::PageDown, Modifiers::NONE);
    assert!(h.state.active_cell().unwrap().0 > 5);
    // Tab shows the record view (DBeaver) and keeps the cell; Shift+Tab moves left.
    let before = h.state.active_cell();
    h.key(Key::Tab, Modifiers::NONE);
    assert!(h.actions.contains(&GridAction::SwitchView(crate::grid::ResultViewKind::Record)), "{:?}", h.actions);
    assert_eq!(h.state.active_cell(), before);
    h.key(Key::Tab, Modifiers::SHIFT);
    assert_eq!(h.state.active_cell().map(|c| c.1), before.map(|c| c.1.saturating_sub(1)));
    h.key(Key::A, Modifiers::COMMAND);
    assert!(h.state.selection().is_all());
}

#[test]
fn copy_paste_edit_and_undo() {
    let mut h = GridHarness::new(editable_model(10));
    h.click_at(h.cell_center(1, 1), PointerButton::Primary, Modifiers::NONE);
    h.frame(vec![Event::Copy]);
    assert_eq!(h.copied().as_deref(), Some("name 1"));
    // Paste a 2x1 block at (1, name).
    h.frame(vec![Event::Paste("x\ny".into())]);
    assert_eq!(h.model.cell(1, 1), Some(Value::text("x")));
    assert_eq!(h.model.cell(2, 1), Some(Value::text("y")));
    // Paste beyond the end adds rows.
    h.state.select_cell(9, 1);
    h.frame(vec![Event::Paste("a\nb\nc".into())]);
    assert_eq!(h.model.row_count(), 12);
    assert_eq!(h.model.row_state(11), RowState::Inserted);
    // Invalid paste into an integer column is rejected as a whole.
    h.state.select_cell(0, 0);
    h.frame(vec![Event::Paste("1\nnope".into())]);
    assert_eq!(h.model.cell(0, 0), Some(Value::Int(0)));
    // Typing starts editing; Enter commits.
    h.state.select_cell(3, 1);
    h.frame(vec![]);
    h.frame(vec![Event::Text("z".into())]);
    assert!(h.state.is_editing());
    h.frame(vec![]);
    h.frame(vec![Event::Text("q".into())]);
    h.key(Key::Enter, Modifiers::NONE);
    assert!(!h.state.is_editing());
    assert_eq!(h.model.cell(3, 1), Some(Value::text("zq")));
    assert_eq!(h.model.row_state(3), RowState::Modified);
    h.key(Key::Z, Modifiers::COMMAND);
    assert_eq!(h.model.cell(3, 1), Some(Value::text("name 3")));
}

#[test]
fn editor_validation_keeps_editor_open() {
    let mut h = GridHarness::new(editable_model(5));
    h.click_at(h.cell_center(0, 0), PointerButton::Primary, Modifiers::NONE);
    h.frame(vec![Event::Text("x".into())]);
    assert!(h.state.is_editing());
    h.frame(vec![]);
    h.key(Key::Enter, Modifiers::NONE);
    assert!(h.state.is_editing(), "invalid integer keeps the editor open");
    assert!(h.state.rt.editing.as_ref().and_then(|e| e.error.clone()).is_some());
    h.key(Key::Escape, Modifiers::NONE);
    assert!(!h.state.is_editing());
    assert_eq!(h.model.cell(0, 0), Some(Value::Int(0)));
    assert!(!h.model.has_changes());
}

#[test]
fn bool_toggle_delete_duplicate_and_set_null() {
    let mut h = GridHarness::new(editable_model(5));
    h.click_at(h.cell_center(0, 2), PointerButton::Primary, Modifiers::NONE);
    h.frame_mods(vec![Event::Key { key: Key::Space, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }, Event::Text(" ".into())], Modifiers::NONE);
    assert_eq!(h.model.cell(0, 2), Some(Value::Bool(false)), "space toggles booleans");
    assert!(!h.state.is_editing());
    h.key(Key::D, Modifiers::COMMAND);
    assert_eq!(h.model.row_count(), 6);
    assert_eq!(h.model.row_state(1), RowState::Inserted);
    h.state.select_cell(3, 1);
    h.key(Key::Delete, Modifiers::SHIFT);
    assert_eq!(h.model.cell(3, 1), Some(Value::Null));
    h.state.select_cell(4, 0);
    h.key(Key::Delete, Modifiers::NONE);
    assert_eq!(h.model.row_state(4), RowState::Deleted);
    // Rows: 0 and 3 modified, one inserted, one deleted.
    assert_eq!(h.model.pending_counts().total(), 4);
}

#[test]
fn read_only_results_refuse_edits_with_reason() {
    let mut m = editable_model(3);
    m.set_editability(Err(ReadOnlyReason::NoKey));
    let mut h = GridHarness::new(m);
    h.click_at(h.cell_center(0, 1), PointerButton::Primary, Modifiers::NONE);
    h.key(Key::Enter, Modifiers::NONE);
    assert!(!h.state.is_editing());
    assert!(h.actions.iter().any(|a| matches!(a, GridAction::Notify { error: true, .. })));
}

#[test]
fn fetch_more_is_emitted_near_the_end() {
    let mut m = editable_model(30);
    m.fetch.has_more = true;
    // The warm-up frames of the harness already emitted FetchMore once.
    let mut h = GridHarness::new(m);
    assert_eq!(h.state.rt.fetch_requested_at, Some(30));
    // Not repeated while the row count is unchanged.
    for _ in 0..3 {
        h.frame(vec![]);
        assert!(!h.actions.contains(&GridAction::FetchMore));
    }
    // A page arrives: emitted again because the end is still near.
    h.model.store().lock().push(vec![Value::Int(99), Value::Null, Value::Null, Value::Null]);
    h.frame(vec![]);
    assert!(h.actions.contains(&GridAction::FetchMore));
    // Not while fetching.
    h.model.fetch.fetching = true;
    h.model.store().lock().push(vec![Value::Int(100), Value::Null, Value::Null, Value::Null]);
    h.frame(vec![]);
    assert!(!h.actions.contains(&GridAction::FetchMore));
}

#[test]
fn header_sort_client_and_server() {
    let mut h = GridHarness::new(editable_model(50));
    // Click the header of column "id" twice: ascending then descending.
    let border = h.header_border(0);
    let zone = egui::pos2(border.x - 26.0, border.y);
    h.click_at(zone, PointerButton::Primary, Modifiers::NONE);
    h.wait_view_job();
    assert_eq!(h.state.sort, vec![crate::view::SortKey::asc(0)]);
    h.click_at(zone, PointerButton::Primary, Modifiers::NONE);
    h.wait_view_job();
    assert_eq!(h.state.sort, vec![crate::view::SortKey::desc(0)]);
    assert_eq!(h.model.cell(0, 0), Some(Value::Int(49)), "client sort applied");
    // Server mode emits ORDER BY.
    h.state.mode = DataMode::Server;
    h.click_at(zone, PointerButton::Primary, Modifiers::NONE);
    let order = h.actions.iter().find_map(|a| match a {
        GridAction::ServerQueryChanged { order_by, .. } => order_by.clone(),
        _ => None,
    });
    assert_eq!(order.as_deref(), Some(""), "third click clears the sort");
}

#[test]
fn column_resize_and_reorder_by_dragging() {
    let mut h = GridHarness::new(editable_model(10));
    let before = h.state.column_width(1);
    let b = h.header_border(1);
    h.drag(b, egui::pos2(b.x + 60.0, b.y));
    let after = h.state.column_width(1);
    assert!((after - before - 60.0).abs() < 2.0, "width {before} -> {after}");
    // Drag the header of "name" (pos 1) past "category" (pos 3).
    let c1 = egui::pos2(h.cell_center(0, 1).x - 10.0, h.geometry().1.center().y);
    let target = egui::pos2(h.header_border(3).x - 5.0, c1.y);
    h.drag(c1, target);
    assert_eq!(h.state.visible_columns(), vec![0, 2, 3, 1]);
}

#[test]
fn row_header_selection_and_context_menu() {
    let mut h = GridHarness::new(editable_model(20));
    let (_, _, rownums, _) = h.geometry();
    let y = h.cell_center(2, 0).y;
    h.click_at(egui::pos2(rownums.center().x, y), PointerButton::Primary, Modifiers::NONE);
    assert!(h.state.selection().is_row_selected(2));
    let y5 = h.cell_center(5, 0).y;
    h.click_at(egui::pos2(rownums.center().x, y5), PointerButton::Primary, Modifiers::SHIFT);
    assert_eq!(h.state.selected_rows(20, 100), vec![2, 3, 4, 5]);
    // Secondary click on a cell outside the selection selects it and opens the menu.
    h.click_at(h.cell_center(10, 1), PointerButton::Secondary, Modifiers::NONE);
    assert_eq!(h.state.active_cell(), Some((10, 1)));
    assert!(egui::Popup::is_any_open(&h.ui.ctx), "context menu is open");
}

#[test]
fn foreign_key_navigation_with_command_click() {
    let mut m = editable_model(5);
    m.set_foreign_keys(vec![ForeignKeyLink {
        name: None,
        columns: vec![3],
        ref_table: dbiji_core::ObjectRef::table(Some("app".into()), "categories"),
        ref_columns: vec!["id".into()],
    }]);
    let mut h = GridHarness::new(m);
    // The action is emitted on the frame of the press.
    let p = h.cell_center(2, 3);
    h.frame_mods(vec![Event::PointerMoved(p)], Modifiers::COMMAND);
    h.frame_mods(vec![crate::test_support::press(p, PointerButton::Primary, Modifiers::COMMAND)], Modifiers::COMMAND);
    let found = h.actions.iter().any(|a| matches!(a, GridAction::NavigateForeignKey { values, .. } if values == &vec![Value::Int(3)]));
    assert!(found, "{:?}", h.actions);
    // A plain click only selects.
    h.frame(vec![crate::test_support::release(p, PointerButton::Primary, Modifiers::NONE)]);
    h.click_at(h.cell_center(1, 3), PointerButton::Primary, Modifiers::NONE);
    assert!(!h.actions.iter().any(|a| matches!(a, GridAction::NavigateForeignKey { .. })));
    assert_eq!(h.state.active_cell(), Some((1, 3)));
}

#[test]
fn dictionary_column_opens_dropdown() {
    let mut m = editable_model(5);
    let mut d = std::collections::HashMap::new();
    d.insert("1".to_string(), "Books".to_string());
    d.insert("2".to_string(), "Music".to_string());
    m.set_dictionary(3, d);
    let mut h = GridHarness::new(m);
    h.click_at(h.cell_center(0, 3), PointerButton::Primary, Modifiers::NONE);
    h.key(Key::Enter, Modifiers::NONE);
    assert!(h.state.rt.dropdown.is_some());
    h.frame(vec![]);
    // Typing filters; Enter picks the first match.
    h.frame(vec![Event::Text("mus".into())]);
    h.key(Key::Enter, Modifiers::NONE);
    h.frame(vec![]);
    assert_eq!(h.model.cell(0, 3), Some(Value::Int(2)));
    assert!(h.state.rt.dropdown.is_none());
}

#[test]
fn advanced_copy_dialog_and_formats() {
    let mut h = GridHarness::new(editable_model(5));
    h.click_at(h.cell_center(0, 0), PointerButton::Primary, Modifiers::NONE);
    h.key(Key::ArrowDown, Modifiers::SHIFT);
    h.key(Key::ArrowRight, Modifiers::SHIFT);
    h.frame_mods(vec![Event::Copy], Modifiers::SHIFT | Modifiers::COMMAND);
    assert!(h.state.rt.copy_dialog);
    let opts = crate::copy::CopyOptions { format: crate::copy::CopyFormat::Markdown, ..Default::default() };
    let (text, cells) = DataGrid::copy_text(&h.model, &h.state, &h.options, &opts).unwrap();
    assert_eq!(cells, 4);
    assert_eq!(text, "| id | name |\n| ---: | --- |\n| 0 | name 0 |\n| 1 | name 1 |");
}

#[test]
fn accessibility_label_describes_grid_and_cell() {
    let mut h = GridHarness::new(editable_model(1000));
    h.ui.ctx.enable_accesskit();
    h.click_at(h.cell_center(1, 1), PointerButton::Primary, Modifiers::NONE);
    h.frame(vec![]);
    let update = h.output.accesskit_update.clone().expect("accesskit tree update");
    let labels: Vec<String> = update.nodes.iter().filter_map(|(_, n)| n.label().map(str::to_string)).collect();
    let grid_label = labels.iter().find(|l| l.starts_with("Data grid")).cloned().unwrap_or_default();
    assert!(grid_label.contains("1,000 rows"), "{labels:?}");
    assert!(grid_label.contains("name = name 1"), "{grid_label}");
}

#[test]
fn virtual_column_dialog_adds_column() {
    let mut h = GridHarness::new(editable_model(3));
    h.state.rt.virtual_dialog = Some(super::state::VirtualColumnDialog { name: "twice".into(), expression: "id * 2".into(), ..Default::default() });
    h.frame(vec![]);
    // Simulate pressing "Add": apply through the model API used by the dialog.
    let idx = h.model.add_virtual_column("twice", "id * 2").unwrap();
    h.state.sync_columns(&h.model);
    h.frame(vec![]);
    assert_eq!(h.model.cell(2, idx), Some(Value::Int(4)));
    assert!(h.state.visible_columns().contains(&idx));
}

#[test]
fn generated_sql_window() {
    let mut h = GridHarness::new(editable_model(3));
    h.click_at(h.cell_center(1, 1), PointerButton::Primary, Modifiers::NONE);
    let rows = h.state.selected_rows(3, 10);
    let sql = h.model.generate_rows_sql(SqlKind::Delete, &rows, &dbiji_core::dialect::GenericDialect::new(), "app.t");
    assert_eq!(sql, "DELETE FROM app.t WHERE id = 1;");
}

#[test]
fn frames_do_not_wait_for_a_busy_store() {
    let mut h = GridHarness::new(editable_model(50));
    h.click_at(h.cell_center(0, 1), PointerButton::Primary, Modifiers::NONE);
    assert!(h.state.rt.body_cache.is_some(), "body shapes are cached");
    let store = h.model.store().clone();
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let guard = store.lock();
        let _ = locked_tx.send(());
        let _ = release_rx.recv_timeout(std::time::Duration::from_secs(5));
        drop(guard);
    });
    locked_rx.recv().unwrap();
    // The fetch task holds the lock: frames still complete quickly (cached body).
    let start = std::time::Instant::now();
    for _ in 0..3 {
        h.key(Key::ArrowDown, Modifiers::NONE);
    }
    let elapsed = start.elapsed();
    let _ = release_tx.send(());
    holder.join().unwrap();
    assert!(elapsed < std::time::Duration::from_millis(500), "frames blocked for {elapsed:?}");
    // Keyboard navigation kept working while the store was busy.
    h.frame(vec![]);
    assert_eq!(h.state.active_cell(), Some((3, 1)));
}

#[test]
fn insert_before_copy_adjacent_uuid_and_columns() {
    let mut h = GridHarness::new(editable_model(5));
    let cmd_alt = Modifiers::COMMAND | Modifiers::ALT;
    h.click_at(h.cell_center(0, 0), PointerButton::Primary, Modifiers::NONE);
    // Add a row before row 2 (Shift+Insert) and before the first row.
    h.state.select_cell(2, 1);
    h.frame(vec![]);
    h.key(Key::Insert, Modifiers::SHIFT);
    assert_eq!(h.model.row_count(), 6);
    assert_eq!(h.model.row_state(2), RowState::Inserted);
    assert_eq!(h.state.active_cell().map(|a| a.0), Some(2));
    assert_eq!(h.model.cell(3, 1), Some(Value::text("name 2")), "the old row 2 moved down");
    h.state.select_cell(0, 1);
    h.frame(vec![]);
    h.key(Key::Insert, Modifiers::SHIFT);
    assert_eq!(h.model.row_state(0), RowState::Inserted);
    assert_eq!(h.model.cell(1, 1), Some(Value::text("name 0")));
    // Copy from the row above / below.
    h.state.select_cell(4, 1); // stored "name 2"
    h.frame(vec![]);
    h.key(Key::ArrowUp, cmd_alt);
    assert_eq!(h.model.cell(4, 1), h.model.cell(3, 1), "copied from the row above");
    h.key(Key::ArrowDown, cmd_alt);
    assert_eq!(h.model.cell(4, 1), Some(Value::text("name 3")), "copied from the row below");
    // A UUID.
    h.state.request(GridCommand::GenerateUuid);
    h.frame(vec![]);
    let v = h.model.cell(4, 1).unwrap().to_text().into_owned();
    assert_eq!(v.len(), 36);
    assert!(uuid::Uuid::parse_str(&v).is_ok(), "{v}");
    // Move the active column right / left.
    h.state.select_cell(0, 0);
    h.frame(vec![]);
    let before = h.state.visible_columns();
    h.key(Key::ArrowRight, cmd_alt);
    let after = h.state.visible_columns();
    assert_eq!((after[0], after[1]), (before[1], before[0]));
    assert_eq!(h.state.active_cell().map(|a| a.1), Some(before[0]), "the moved column stays active");
    h.key(Key::ArrowLeft, cmd_alt);
    assert_eq!(h.state.visible_columns(), before);
}

#[test]
fn fit_hide_empty_highlight_and_zoom() {
    let mut h = GridHarness::new(editable_model(5));
    // Fit to screen: the columns fill the body.
    h.state.request(GridCommand::FitColumnsToScreen);
    h.frame(vec![]);
    h.frame(vec![]);
    let (body, ..) = h.geometry();
    let total: f32 = h.state.visible_columns().iter().map(|&c| h.state.column_width(c)).sum();
    assert!((total - (body.width() - 2.0)).abs() < 2.0, "{total} vs {}", body.width());
    // A column with only NULLs is hidden.
    for r in 0..5 {
        h.model.set_cells(vec![(r, 2, crate::model::CellValue::Value(Value::Null))]).unwrap();
    }
    let visible = h.state.visible_columns().len();
    h.state.request(GridCommand::HideEmptyColumns);
    h.frame(vec![]);
    assert_eq!(h.state.visible_columns().len(), visible - 1);
    assert!(!h.state.visible_columns().contains(&2));
    // Highlights and zoom are part of the saved layout.
    h.state.set_highlight("name", Some("name 1".into()), Some([230, 70, 70]));
    h.state.set_highlight("name", Some("name 1".into()), Some([60, 180, 90]));
    assert_eq!(h.state.highlights.len(), 1, "one colour per value");
    h.state.zoom_by(Some(0.2));
    h.frame(vec![]);
    let saved = serde_json::to_string(&h.state).unwrap();
    let back: DataGridState = serde_json::from_str(&saved).unwrap();
    assert_eq!(back.highlights, h.state.highlights);
    assert_eq!(back.zoom, Some(1.2));
    h.state.zoom_by(None);
    assert_eq!(h.state.zoom, None);
    h.state.set_highlight("name", Some("name 1".into()), None);
    assert!(h.state.highlights.is_empty());
}

#[test]
fn header_menu_button_opens_the_column_menu() {
    let mut h = GridHarness::new(editable_model(20));
    let border = h.header_border(1);
    h.click_at(egui::pos2(border.x - 10.0, border.y), PointerButton::Primary, Modifiers::NONE);
    assert_eq!(h.state.rt.header_popup.map(|p| p.0), Some(1), "▾ of the second column");
    assert!(h.state.sort.is_empty(), "the ▾ does not sort");
    // It stays open on the next frames; a click elsewhere closes it.
    h.frame(vec![]);
    assert!(h.state.rt.header_popup.is_some());
    h.click_at(h.cell_center(5, 0), PointerButton::Primary, Modifiers::NONE);
    h.frame(vec![]);
    assert!(h.state.rt.header_popup.is_none());
}

#[test]
fn open_editor_follows_its_record_when_the_view_changes() {
    let mut h = GridHarness::new(editable_model(10));
    // A filtered view shows store rows 5.. first; edit display row 0 (store row 5).
    h.model.set_view(Some((5..10).collect()));
    h.click_at(h.cell_center(0, 1), PointerButton::Primary, Modifiers::NONE);
    h.frame(vec![Event::Text("z".into())]);
    assert!(h.state.is_editing());
    // The filter is cleared while the editor is open: display row 0 is store row 0 now.
    h.model.clear_view();
    h.frame(vec![]);
    h.key(Key::Enter, Modifiers::NONE);
    assert!(!h.state.is_editing());
    assert_eq!(h.model.cell(5, 1), Some(Value::text("z")), "the edited record");
    assert_eq!(h.model.cell(0, 1), Some(Value::text("name 0")), "not the row now shown at that position");
}

#[test]
fn copied_null_pastes_back_as_its_own_row() {
    let mut h = GridHarness::new(editable_model(10));
    h.model.set_value(1, 1, Value::Null).unwrap();
    h.click_at(h.cell_center(0, 1), PointerButton::Primary, Modifiers::NONE);
    h.key(Key::ArrowDown, Modifiers::SHIFT);
    h.frame(vec![Event::Copy]);
    let text = h.copied().unwrap();
    assert_eq!(text, "name 0\n");
    // A two-cell selection gets the two copied values, not the first one twice.
    h.state.select_cell(5, 1);
    h.key(Key::ArrowDown, Modifiers::SHIFT);
    h.frame(vec![Event::Paste(text)]);
    assert_eq!(h.model.cell(5, 1), Some(Value::text("name 0")));
    assert_eq!(h.model.cell(6, 1), Some(Value::Null));
}

#[test]
fn columns_fit_header_and_first_values_until_sized_by_hand() {
    let long = "2024-07-30 19:18:46.838 +0700 and some more text";
    let mut m = editable_model(10);
    m.set_value(3, 1, Value::text(long)).unwrap();
    let mut h = GridHarness::new(m);
    h.frame(vec![]);
    h.frame(vec![]);
    // "flag" (booleans) is no wider than its header needs; "name" fits its longest value.
    let header_only = h.state.column_width(2);
    assert!(header_only < crate::grid::state::default_width("flag", dbiji_core::DataKind::Bool) + 40.0, "{header_only}");
    let name = h.state.column_width(1);
    assert!(name > crate::grid::state::default_width("name", dbiji_core::DataKind::Text), "{name}");
    assert!(name <= crate::grid::state::MAX_INITIAL_WIDTH);
    // A width set by hand is kept when the rows change.
    h.state.set_column_width(1, 90.0);
    h.model.set_value(4, 1, Value::text(format!("{long} {long}"))).unwrap();
    h.frame(vec![]);
    assert_eq!(h.state.column_width(1), 90.0);
}
