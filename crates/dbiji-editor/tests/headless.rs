//! Headless egui tests: drive `SqlEditor` through real `Context` frames.

use dbiji_editor::{
    BasicSqlLanguage, CodeView, ContextAction, Diagnostic, EditorEvent, EditorLanguage,
    EditorOptions, EditorState, EditorTheme, EntryKind, Severity, SqlEditor, TableInfo,
    consume_shortcut_with_text,
};
use egui::{Event, ImeEvent, Key, Modifiers, PointerButton, Pos2, RawInput, Rect, vec2};

fn cmd() -> Modifiers {
    if cfg!(target_os = "macos") {
        Modifiers::MAC_CMD.plus(Modifiers::COMMAND)
    } else {
        Modifiers::CTRL.plus(Modifiers::COMMAND)
    }
}

struct Harness {
    ctx: egui::Context,
    state: EditorState,
    lang: BasicSqlLanguage,
    options: EditorOptions,
    theme: EditorTheme,
    time: f64,
    events: Vec<EditorEvent>,
    copied: Vec<String>,
    repaint_delay: std::time::Duration,
    modifiers: Modifiers,
    sent_modifiers: Modifiers,
    accesskit: Option<egui::accesskit::TreeUpdate>,
    screen: egui::Vec2,
}

impl Harness {
    fn new(text: &str) -> Self {
        let lang = BasicSqlLanguage::new().with_tables(vec![
            TableInfo {
                name: "users".into(),
                columns: vec!["id".into(), "name".into(), "email".into()],
            },
            TableInfo {
                name: "orders".into(),
                columns: vec!["id".into(), "user_id".into(), "total".into()],
            },
        ]);
        let mut h = Self {
            ctx: egui::Context::default(),
            state: EditorState::new("test", text.to_owned()),
            lang,
            options: EditorOptions {
                cursor_blink: false,
                ..EditorOptions::default()
            },
            theme: EditorTheme::dark(),
            time: 0.0,
            events: Vec::new(),
            copied: Vec::new(),
            repaint_delay: std::time::Duration::ZERO,
            modifiers: Modifiers::NONE,
            sent_modifiers: Modifiers::NONE,
            accesskit: None,
            screen: vec2(900.0, 600.0),
        };
        h.frame(vec![]);
        h
    }

    fn frame(&mut self, events: Vec<Event>) -> bool {
        self.time += 1.0 / 60.0;
        let mut all = Vec::new();
        if self.modifiers != self.sent_modifiers {
            all.push(Event::ModifiersChanged(self.modifiers));
            self.sent_modifiers = self.modifiers;
        }
        all.extend(events);
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, self.screen)),
            time: Some(self.time),
            focused: true,
            events: all,
            ..Default::default()
        };
        let mut changed = false;
        let mut frame_events = Vec::new();
        let (state, lang, options, theme) =
            (&mut self.state, &self.lang, &self.options, &self.theme);
        let out = self.ctx.run_ui(input, |ui| {
            let r = SqlEditor::new(state, lang, options, theme).show(ui);
            changed |= r.changed;
            frame_events.extend(r.events);
        });
        for c in &out.platform_output.commands {
            if let egui::OutputCommand::CopyText(t) = c {
                self.copied.push(t.clone());
            }
        }
        if let Some(update) = out.platform_output.accesskit_update.clone() {
            self.accesskit = Some(update);
        }
        self.repaint_delay = out
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map_or(std::time::Duration::MAX, |v| v.repaint_delay);
        out.drop_without_applying_deltas();
        self.events.extend(frame_events);
        changed
    }

    fn focus(&mut self) {
        self.state.request_focus(&self.ctx);
        self.frame(vec![]);
        self.frame(vec![]);
        assert!(self.state.has_focus(&self.ctx));
    }

    fn type_str(&mut self, s: &str) {
        for c in s.chars() {
            self.frame(vec![Event::Text(c.to_string())]);
        }
    }

    fn key(&mut self, key: Key, modifiers: Modifiers) {
        self.modifiers = modifiers;
        let press = Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        let release = Event::Key {
            key,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers,
        };
        self.frame(vec![press, release]);
        self.modifiers = Modifiers::NONE;
    }

    fn click(&mut self, pos: Pos2, modifiers: Modifiers) {
        self.modifiers = modifiers;
        self.frame(vec![Event::PointerMoved(pos)]);
        self.frame(vec![Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed: true,
            modifiers,
        }]);
        self.frame(vec![Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed: false,
            modifiers,
        }]);
        self.modifiers = Modifiers::NONE;
    }

    fn row_height(&self) -> f32 {
        let font = egui::FontId::new(self.options.font_size, self.options.font_family.clone());
        let h = self.ctx.fonts_mut(|f| f.row_height(&font));
        (h + self.options.line_spacing).round()
    }

    fn text(&self) -> &str {
        self.state.text()
    }
}

#[test]
fn typing_newline_and_smart_indent() {
    let mut h = Harness::new("");
    h.focus();
    h.type_str("select");
    assert_eq!(h.text(), "select");
    h.key(Key::Enter, Modifiers::NONE);
    h.type_str("  a,");
    h.key(Key::Enter, Modifiers::NONE);
    h.type_str("b");
    assert_eq!(h.text(), "select\n  a,\n  b");
    assert_eq!(h.state.line_col(), (3, 4));
    // Enter between parentheses splits onto three lines.
    h.type_str(" in (");
    assert_eq!(h.text(), "select\n  a,\n  b in ()");
    h.key(Key::Enter, Modifiers::NONE);
    assert_eq!(h.text(), "select\n  a,\n  b in (\n      \n  )");
    assert_eq!(h.state.line_col(), (4, 7));
}

#[test]
fn auto_close_and_type_over() {
    let mut h = Harness::new("");
    h.focus();
    h.type_str("count(");
    assert_eq!(h.text(), "count()");
    assert_eq!(h.state.cursor(), 6);
    h.type_str("*)");
    assert_eq!(h.text(), "count(*)");
    assert_eq!(h.state.cursor(), 8);
    h.type_str(" = '");
    assert_eq!(h.text(), "count(*) = ''");
    h.type_str("x'");
    assert_eq!(h.text(), "count(*) = 'x'");
    // Backspace inside an empty auto-inserted pair removes both chars.
    h.type_str(" (");
    h.key(Key::Backspace, Modifiers::NONE);
    assert_eq!(h.text(), "count(*) = 'x' ");
    // Wrapping a selection.
    let ctx = h.ctx.clone();
    h.state.select(&ctx, 12..13);
    h.type_str("(");
    assert_eq!(h.text(), "count(*) = '(x)' ");
    assert_eq!(h.state.selection(), Some(13..14));
    // Disabled auto close inserts literally.
    h.options.auto_close_brackets = false;
    h.state.set_cursor(&ctx, h.text().len());
    h.type_str("(");
    assert!(h.text().ends_with("' ("));
}

#[test]
fn undo_redo_and_modified_flag() {
    let mut h = Harness::new("select 1");
    h.focus();
    assert!(!h.state.is_modified());
    let ctx = h.ctx.clone();
    h.state.set_cursor(&ctx, 8);
    h.frame(vec![]);
    h.type_str(" from t");
    assert_eq!(h.text(), "select 1 from t");
    assert!(h.state.is_modified());
    h.key(Key::Z, cmd());
    assert_eq!(h.text(), "select 1 from");
    h.key(Key::Z, cmd());
    assert_eq!(h.text(), "select 1");
    assert!(!h.state.is_modified());
    h.key(Key::Z, cmd().plus(Modifiers::SHIFT));
    assert_eq!(h.text(), "select 1 from");
    h.state.mark_saved();
    assert!(!h.state.is_modified());
}

#[test]
fn tab_indent_outdent_and_comment_toggle() {
    let mut h = Harness::new("select a\nfrom t\nwhere x");
    h.focus();
    let ctx = h.ctx.clone();
    h.state.select(&ctx, 0..15);
    h.frame(vec![]);
    h.key(Key::Tab, Modifiers::NONE);
    assert_eq!(h.text(), "    select a\n    from t\nwhere x");
    h.key(Key::Tab, Modifiers::SHIFT);
    assert_eq!(h.text(), "select a\nfrom t\nwhere x");
    h.key(Key::Slash, cmd());
    assert_eq!(h.text(), "-- select a\n-- from t\nwhere x");
    h.key(Key::Slash, cmd());
    assert_eq!(h.text(), "select a\nfrom t\nwhere x");
    // Tab without a multi-line selection inserts spaces to the next stop.
    h.state.set_cursor(&ctx, 2);
    h.frame(vec![]);
    h.key(Key::Tab, Modifiers::NONE);
    assert_eq!(&h.text()[..8], "se  lect");
}

#[test]
fn line_commands() {
    let mut h = Harness::new("one\ntwo\nthree");
    h.focus();
    let ctx = h.ctx.clone();
    h.state.set_cursor(&ctx, 5);
    h.frame(vec![]);
    h.key(Key::ArrowUp, Modifiers::ALT);
    assert_eq!(h.text(), "two\none\nthree");
    h.key(Key::ArrowDown, Modifiers::ALT);
    h.key(Key::ArrowDown, Modifiers::ALT);
    assert_eq!(h.text(), "one\nthree\ntwo");
    h.key(Key::D, cmd());
    assert_eq!(h.text(), "one\nthree\ntwo\ntwo");
    h.key(Key::K, cmd().plus(Modifiers::SHIFT));
    assert_eq!(h.text(), "one\nthree\ntwo");
    h.key(Key::X, cmd().plus(Modifiers::SHIFT));
    assert_eq!(h.text(), "one\nthree\nTWO");
    h.key(Key::Y, cmd().plus(Modifiers::SHIFT));
    assert_eq!(h.text(), "one\nthree\ntwo");
}

#[test]
fn keyboard_navigation() {
    let mut h = Harness::new("select foo_bar\n    from t\nx");
    h.focus();
    let ctx = h.ctx.clone();
    h.state.set_cursor(&ctx, 0);
    h.frame(vec![]);
    let word = if cfg!(target_os = "macos") {
        Modifiers::ALT
    } else {
        Modifiers::CTRL
    };
    h.key(Key::ArrowRight, word);
    assert_eq!(h.state.cursor(), 6);
    h.key(Key::ArrowRight, word);
    assert_eq!(h.state.cursor(), 14);
    h.key(Key::ArrowDown, Modifiers::NONE);
    assert_eq!(h.state.line_col().0, 2);
    h.key(Key::Home, Modifiers::NONE);
    assert_eq!(
        h.state.line_col(),
        (2, 5),
        "smart home goes to first non-blank"
    );
    h.key(Key::Home, Modifiers::NONE);
    assert_eq!(h.state.line_col(), (2, 1));
    h.key(Key::End, Modifiers::SHIFT);
    assert_eq!(h.state.selected_text(), Some("    from t"));
    h.key(Key::ArrowLeft, Modifiers::NONE);
    assert_eq!(
        h.state.line_col(),
        (2, 1),
        "left collapses to selection start"
    );
    h.key(Key::ArrowDown, Modifiers::NONE);
    h.key(Key::ArrowDown, Modifiers::NONE);
    assert_eq!(h.state.cursor(), h.text().len());
    h.key(Key::A, cmd());
    assert_eq!(h.state.selection(), Some(0..h.text().len()));
    h.key(Key::Escape, Modifiers::NONE);
    assert_eq!(h.state.selection(), None);
}

#[test]
fn clipboard_cut_copy_paste() {
    let mut h = Harness::new("select 1\nfrom t");
    h.focus();
    let ctx = h.ctx.clone();
    h.state.select(&ctx, 0..6);
    h.frame(vec![Event::Copy]);
    assert_eq!(h.copied.last().map(String::as_str), Some("select"));
    h.frame(vec![Event::Cut]);
    assert_eq!(h.text(), " 1\nfrom t");
    h.frame(vec![Event::Paste("SELECT\r\n".into())]);
    assert_eq!(h.text(), "SELECT\n 1\nfrom t");
    // Copy without selection copies the line.
    h.state.set_cursor(&ctx, h.text().len());
    h.frame(vec![Event::Copy]);
    assert_eq!(h.copied.last().map(String::as_str), Some("from t\n"));
}

#[test]
fn completion_manual_auto_filter_accept() {
    let mut h = Harness::new("");
    h.focus();
    h.type_str("SELECT * FROM us");
    assert!(!h.state.is_completion_open());
    h.key(Key::Space, Modifiers::CTRL);
    assert!(h.state.is_completion_open());
    h.key(Key::Enter, Modifiers::NONE);
    assert_eq!(h.text(), "SELECT * FROM users");
    assert!(!h.state.is_completion_open());
    assert!(h.events.iter().any(|e| matches!(e, EditorEvent::CompletionAccepted(c) if c.label == "users" && c.kind == EntryKind::Table)));

    // `.` triggers immediately; typing filters without re-querying.
    h.type_str(" u WHERE u.");
    assert!(h.state.is_completion_open());
    h.type_str("na");
    assert!(h.state.is_completion_open());
    h.key(Key::Tab, Modifiers::NONE);
    assert_eq!(h.text(), "SELECT * FROM users u WHERE u.name");

    // Escape closes; non-matching prefix closes.
    h.type_str(" AND u.");
    assert!(h.state.is_completion_open());
    h.key(Key::Escape, Modifiers::NONE);
    assert!(!h.state.is_completion_open());
    h.key(Key::Space, Modifiers::CTRL);
    h.type_str("zzz");
    assert!(!h.state.is_completion_open());

    // Function entries move the cursor back between the parentheses.
    h.key(Key::Enter, Modifiers::NONE);
    h.type_str("coun");
    h.key(Key::Space, Modifiers::CTRL);
    h.key(Key::Enter, Modifiers::NONE);
    assert!(h.text().ends_with("count()"));
    assert_eq!(h.state.cursor(), h.text().len() - 1);
}

#[test]
fn completion_auto_trigger_on_letters_with_delay() {
    let mut h = Harness::new("");
    h.options.auto_complete_on_letters = true;
    h.options.auto_complete_delay_ms = 100;
    h.focus();
    h.type_str("sel");
    assert!(!h.state.is_completion_open(), "waits for the delay");
    for _ in 0..10 {
        h.frame(vec![]);
    }
    assert!(h.state.is_completion_open());
    h.key(Key::ArrowDown, Modifiers::NONE);
    h.key(Key::Escape, Modifiers::NONE);
    assert!(!h.state.is_completion_open());
}

#[test]
fn find_bar_navigation_and_replace() {
    let mut h = Harness::new("select a from a where a = 1");
    h.focus();
    h.key(Key::F, cmd());
    assert!(h.state.is_find_open());
    h.frame(vec![]);
    h.type_str("a");
    // Typing went into the find field, not into the document.
    assert_eq!(h.text(), "select a from a where a = 1");
    h.key(Key::Enter, Modifiers::NONE);
    let first = h.state.selection();
    assert!(first.is_some());
    h.key(Key::Enter, Modifiers::NONE);
    assert_ne!(h.state.selection(), first);
    h.key(Key::Escape, Modifiers::NONE);
    h.frame(vec![]);
    assert!(!h.state.is_find_open());
    h.frame(vec![]);
    assert!(h.state.has_focus(&h.ctx), "focus returns to the editor");
    // Find next from the editor wraps around.
    h.key(Key::G, cmd());
    assert!(h.state.selection().is_some());
}

#[test]
fn read_only_mode() {
    let mut h = Harness::new("select 1");
    h.options.read_only = true;
    h.focus();
    h.type_str("xyz");
    h.key(Key::Backspace, Modifiers::NONE);
    h.key(Key::Enter, Modifiers::NONE);
    h.frame(vec![Event::Paste("p".into())]);
    assert_eq!(h.text(), "select 1");
    h.key(Key::End, Modifiers::SHIFT);
    assert_eq!(h.state.selected_text(), Some("select 1"));
    h.frame(vec![Event::Copy]);
    assert_eq!(h.copied.last().map(String::as_str), Some("select 1"));
}

#[test]
fn ime_composition_is_one_undo_step() {
    let mut h = Harness::new("x ");
    h.focus();
    let ctx = h.ctx.clone();
    h.state.set_cursor(&ctx, 2);
    h.frame(vec![]);
    h.frame(vec![Event::Ime(ImeEvent::Preedit {
        text: "n".into(),
        active_range_chars: None,
    })]);
    h.frame(vec![Event::Ime(ImeEvent::Preedit {
        text: "ni".into(),
        active_range_chars: None,
    })]);
    assert_eq!(h.text(), "x ni");
    h.frame(vec![Event::Ime(ImeEvent::Commit("你".into()))]);
    assert_eq!(h.text(), "x 你");
    assert_eq!(h.state.cursor(), h.text().len());
    h.key(Key::Z, cmd());
    assert_eq!(h.text(), "x ");
    // A commit without composition behaves like typing (auto-close applies).
    h.frame(vec![Event::Ime(ImeEvent::Commit("(".into()))]);
    assert_eq!(h.text(), "x ()");
}

#[test]
fn ime_macos_sequence_clear_then_commit() {
    let mut h = Harness::new("a");
    h.focus();
    let ctx = h.ctx.clone();
    h.state.set_cursor(&ctx, 1);
    h.frame(vec![]);
    h.frame(vec![Event::Ime(ImeEvent::Preedit {
        text: "c".into(),
        active_range_chars: Some(1..1),
    })]);
    h.frame(vec![Event::Ime(ImeEvent::Preedit {
        text: "ce".into(),
        active_range_chars: Some(2..2),
    })]);
    assert_eq!(h.text(), "ace");
    h.frame(vec![
        Event::Ime(ImeEvent::Preedit {
            text: String::new(),
            active_range_chars: None,
        }),
        Event::Ime(ImeEvent::Commit("测试".into())),
    ]);
    assert_eq!(h.text(), "a测试");
    h.key(Key::Z, cmd());
    assert_eq!(h.text(), "a", "the whole composition is one undo step");
    // Clearing a composition without commit (backspace in the IME) leaves the text untouched.
    h.frame(vec![Event::Ime(ImeEvent::Preedit {
        text: "x".into(),
        active_range_chars: None,
    })]);
    h.frame(vec![Event::Ime(ImeEvent::Preedit {
        text: String::new(),
        active_range_chars: None,
    })]);
    assert_eq!(h.text(), "a");
}

#[test]
fn click_during_composition_keeps_preedit_text() {
    let mut h = Harness::new("first\nsecond");
    h.focus();
    let ctx = h.ctx.clone();
    h.state.set_cursor(&ctx, 5);
    h.frame(vec![]);
    h.frame(vec![Event::Ime(ImeEvent::Preedit {
        text: "ab".into(),
        active_range_chars: None,
    })]);
    assert_eq!(h.text(), "firstab\nsecond");
    // Click on the second line: the composition is interrupted.
    let rh = h.row_height();
    h.click(Pos2::new(400.0, 2.0 + rh * 1.5), Modifiers::NONE);
    h.frame(vec![Event::Ime(ImeEvent::Preedit {
        text: "c".into(),
        active_range_chars: None,
    })]);
    h.frame(vec![Event::Ime(ImeEvent::Commit("c".into()))]);
    assert_eq!(h.text(), "firstab\nsecondc");
}

#[test]
fn tab_on_whole_line_selection_indents() {
    let mut h = Harness::new("select 1\nselect 2");
    h.focus();
    let ctx = h.ctx.clone();
    h.state.select(&ctx, 0..9);
    h.frame(vec![]);
    h.key(Key::Tab, Modifiers::NONE);
    assert_eq!(h.text(), "    select 1\nselect 2");
    assert_eq!(h.state.selection(), Some(0..13));
    // A selection inside one line is replaced by the indentation
    // (spaces up to the next tab stop: "sele|ct |2" -> 4 spaces).
    h.state.select(&ctx, 17..20);
    h.frame(vec![]);
    h.key(Key::Tab, Modifiers::NONE);
    assert_eq!(h.text(), "    select 1\nsele    2");
}

#[test]
fn mouse_click_shift_click_and_gutter() {
    let mut h = Harness::new("first line\nsecond line\nthird line");
    h.focus();
    let rh = h.row_height();
    let y = |line: f32| 2.0 + rh * line + rh / 2.0;
    h.click(Pos2::new(400.0, y(1.0)), Modifiers::NONE);
    assert_eq!(h.state.line_col(), (2, 12), "click past the end of line 2");
    h.click(Pos2::new(400.0, y(2.0)), Modifiers::SHIFT);
    assert_eq!(h.state.selected_text(), Some("\nthird line"));
    // Gutter click selects the whole line.
    h.click(Pos2::new(3.0, y(0.0)), Modifiers::NONE);
    assert_eq!(h.state.selected_text(), Some("first line\n"));
}

#[test]
fn events_cursor_focus_and_links() {
    let mut h = Harness::new("select * from users");
    h.focus();
    assert!(h.events.contains(&EditorEvent::FocusChanged(true)));
    h.key(Key::End, Modifiers::NONE);
    assert!(h.events.contains(&EditorEvent::CursorMoved(19)));
    // Command-click on a table name emits LinkClicked.
    let rh = h.row_height();
    let char_w = h
        .ctx
        .fonts_mut(|f| f.glyph_width(&egui::FontId::monospace(13.0), '0'));
    // Gutter: markers, two digits, padding, fold markers.
    let x_users = 12.0 + 2.0 * char_w + 6.0 + 12.0 + 16.0 * char_w;
    h.events.clear();
    h.click(Pos2::new(x_users, 2.0 + rh / 2.0), cmd());
    assert!(
        h.events
            .iter()
            .any(|e| matches!(e, EditorEvent::LinkClicked { text, .. } if text == "users")),
        "events: {:?}",
        h.events
    );
}

#[test]
fn context_actions_and_save_shortcut() {
    let mut h = Harness::new("select 1");
    h.focus();
    let ctx = h.ctx.clone();
    h.state.select(&ctx, 0..6);
    assert!(
        h.state
            .apply_action(&ctx, &h.lang, &h.options, &ContextAction::UpperCase)
    );
    assert_eq!(h.text(), "SELECT 1");
    assert!(
        !h.state
            .apply_action(&ctx, &h.lang, &h.options, &ContextAction::Execute)
    );
    assert!(h.state.apply_action(
        &ctx,
        &h.lang,
        &h.options,
        &ContextAction::ReplaceWord {
            range: 7..8,
            with: "42".into()
        }
    ));
    assert_eq!(h.text(), "SELECT 42");
    h.key(Key::S, cmd());
    assert!(h.events.contains(&EditorEvent::SaveRequested));
    assert!(ContextAction::Execute.label().contains("Execute"));
    assert!(!ContextAction::Format.is_editor_action());
}

#[test]
#[allow(clippy::single_range_in_vec_init)]
fn diagnostics_statement_spell_paint_without_panics() {
    let mut h = Harness::new("select x frm t;\nselect 2;");
    h.focus();
    h.state.set_diagnostics(vec![
        Diagnostic::new(7..8, Severity::Error, "unknown column x"),
        Diagnostic::new(9..12, Severity::Warning, "did you mean FROM?"),
        Diagnostic::new(15..15, Severity::Info, "end"),
    ]);
    h.state.set_statement_highlight(Some(0..15));
    h.state.set_spell_issues(vec![9..12]);
    for _ in 0..3 {
        h.frame(vec![Event::PointerMoved(Pos2::new(120.0, 10.0))]);
    }
    h.type_str("y");
    assert_eq!(h.state.diagnostics().len(), 3);
}

#[test]
fn accesskit_exposes_text_runs_and_accepts_selection_requests() {
    use egui::accesskit::{
        Action, ActionData, ActionRequest, Role, TextPosition, TextSelection, TreeId,
    };
    let mut h = Harness::new("select 1;\nselect 2;");
    h.ctx.enable_accesskit();
    h.focus();
    h.frame(vec![]);
    let update = h.accesskit.take().expect("accesskit update");
    let editor = h.state.id().accesskit_id();
    let node = update
        .nodes
        .iter()
        .find(|(id, _)| *id == editor)
        .map(|(_, n)| n)
        .expect("editor node");
    assert_eq!(node.role(), Role::MultilineTextInput);
    assert_eq!(node.value(), Some("select 1;\nselect 2;"));
    let run = h.state.id().with(0usize).with(0usize).accesskit_id();
    assert!(
        update
            .nodes
            .iter()
            .any(|(id, n)| *id == run && n.role() == Role::TextRun)
    );
    let req = ActionRequest {
        action: Action::SetTextSelection,
        target_tree: TreeId::ROOT,
        target_node: editor,
        data: Some(ActionData::SetTextSelection(TextSelection {
            anchor: TextPosition {
                node: run,
                character_index: 7,
            },
            focus: TextPosition {
                node: run,
                character_index: 9,
            },
        })),
    };
    h.frame(vec![Event::AccessKitActionRequest(req)]);
    assert_eq!(h.state.selected_text(), Some("1;"));
}

#[test]
fn idle_editor_requests_no_repaint() {
    let mut h = Harness::new("select 1;\nselect 2;");
    h.focus();
    for _ in 0..5 {
        h.frame(vec![]);
    }
    assert!(
        h.repaint_delay > std::time::Duration::from_secs(1000),
        "idle frame asked for repaint after {:?}: {:?}",
        h.repaint_delay,
        h.ctx.repaint_causes()
    );
}

#[test]
fn goto_line_scrolls_and_large_document_is_virtualized() {
    let text: String = (0..20_000)
        .map(|i| format!("SELECT {i} FROM t{i}; -- line {i}\n"))
        .collect();
    let mut h = Harness::new(&text);
    h.focus();
    let ctx = h.ctx.clone();
    h.state.goto_line(&ctx, 15_000);
    h.frame(vec![]);
    h.frame(vec![]);
    assert_eq!(h.state.line_col(), (15_000, 1));
    let view = h.state.view_state();
    assert!(
        view.scroll[1] > 100_000.0,
        "scrolled to line 15000: {view:?}"
    );
    h.type_str("x");
    assert!(h.text().contains("xSELECT 14999"));
    // Go-to-line popup via keyboard.
    h.key(Key::L, cmd());
    h.frame(vec![]);
    h.type_str("10");
    h.key(Key::Enter, Modifiers::NONE);
    h.frame(vec![]);
    assert_eq!(h.state.line_col().0, 10);
}

#[test]
fn end_key_reaches_real_end_of_very_long_line() {
    let long = "x".repeat(30_000);
    let mut h = Harness::new(&format!("a\n{long}\nb"));
    h.focus();
    let ctx = h.ctx.clone();
    h.state.set_cursor(&ctx, 2);
    h.frame(vec![]);
    h.key(Key::End, Modifiers::NONE);
    assert_eq!(h.state.cursor(), 2 + 30_000);
    h.type_str("!");
    assert!(h.text().ends_with("x!\nb"));
}

#[test]
fn bulk_operations_on_large_documents_are_fast() {
    let text: String = (0..20_000).map(|i| format!("select {i};\n")).collect();
    let mut h = Harness::new(&text);
    h.focus();
    let started = std::time::Instant::now();
    h.key(Key::A, cmd());
    h.key(Key::Slash, cmd());
    assert!(h.text().starts_with("-- select 0;\n-- select 1;"));
    h.key(Key::Z, cmd());
    assert_eq!(h.text(), text);
    h.key(Key::Z, cmd().plus(Modifiers::SHIFT));
    assert!(h.text().starts_with("-- select 0;"));
    // Replace all through the find bar machinery.
    h.key(Key::Z, cmd());
    h.key(Key::H, cmd());
    h.state.open_find(true);
    h.frame(vec![]);
    let elapsed = started.elapsed();
    assert!(elapsed.as_secs_f64() < 5.0, "bulk edits took {elapsed:?}");
}

#[test]
fn word_wrap_navigation() {
    let long = "word ".repeat(60);
    let mut h = Harness::new(&format!("{long}\nnext"));
    h.options.word_wrap = true;
    h.focus();
    let ctx = h.ctx.clone();
    h.state.set_cursor(&ctx, 0);
    h.frame(vec![]);
    for _ in 0..5 {
        h.frame(vec![]);
    }
    h.key(Key::ArrowDown, Modifiers::NONE);
    assert_eq!(
        h.state.line_col().0,
        1,
        "down moves to the next visual row of the same line"
    );
    assert!(h.state.cursor() > 0);
    h.key(Key::End, Modifiers::NONE);
    assert_eq!(h.state.line_col().0, 1);
    h.type_str("!");
    assert!(h.text().contains('!'));
}

/// Number `N` of the first visible line (lines read "LN ..."), from the
/// first AccessKit text run.
fn first_visible_line(h: &mut Harness) -> usize {
    h.frame(vec![]);
    let update = h.accesskit.take().expect("accesskit update");
    let run = h.state.id().with(0usize).with(0usize).accesskit_id();
    let value = update
        .nodes
        .iter()
        .find(|(id, _)| *id == run)
        .and_then(|(_, n)| n.value())
        .expect("first text run")
        .to_owned();
    value
        .strip_prefix('L')
        .and_then(|v| v.split(' ').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("unexpected first row {value:?}"))
}

#[test]
fn first_visible_line_survives_rewrap_refinement_and_zoom() {
    // Line lengths vary so re-wrapping changes the row count above the view.
    let text: String = (0..4_000)
        .map(|i| format!("L{i} SELECT {}x FROM t;\n", "col, ".repeat(i % 37)))
        .collect();
    let mut h = Harness::new(&text);
    h.options.word_wrap = true;
    h.ctx.enable_accesskit();
    h.focus();
    let ctx = h.ctx.clone();
    h.state.goto_line(&ctx, 3_000);
    for _ in 0..3 {
        h.frame(vec![]);
    }
    let first = first_visible_line(&mut h);
    assert!(first > 2_000, "scrolled down: {first}");
    // Narrower window: everything re-wraps, rows above are estimated and
    // then measured in the background over the next frames.
    h.screen = vec2(420.0, 600.0);
    for _ in 0..60 {
        h.frame(vec![]);
        assert_eq!(first_visible_line(&mut h), first, "during refinement");
    }
    h.screen = vec2(1400.0, 600.0);
    for _ in 0..60 {
        h.frame(vec![]);
    }
    assert_eq!(first_visible_line(&mut h), first, "after widening");
    // Zoom.
    h.options.font_size = 20.0;
    for _ in 0..10 {
        h.frame(vec![]);
    }
    assert_eq!(first_visible_line(&mut h), first, "after zooming");
}

#[test]
fn code_view_is_read_only_and_follows_appends() {
    let ctx = egui::Context::default();
    let lang = BasicSqlLanguage::new();
    let theme = EditorTheme::light();
    let mut text = String::from("CREATE TABLE t (id int);\n");
    let mut copied = Vec::new();
    for frame in 0..6 {
        if frame == 3 {
            text.push_str("-- appended\n");
        }
        let events = if frame == 4 {
            vec![
                Event::Key {
                    key: Key::A,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: cmd(),
                },
                Event::Copy,
            ]
        } else {
            vec![]
        };
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(600.0, 400.0))),
            time: Some(frame as f64 / 60.0),
            events,
            ..Default::default()
        };
        let t = text.clone();
        let out = ctx.run_ui(input, |ui| {
            let r = CodeView::new(&t, &lang, &theme).id_salt("ddl").show(ui);
            if frame == 1 {
                r.response.request_focus();
            }
        });
        for c in &out.platform_output.commands {
            if let egui::OutputCommand::CopyText(s) = c {
                copied.push(s.clone());
            }
        }
        out.drop_without_applying_deltas();
    }
    assert_eq!(
        copied.last().map(String::as_str),
        Some("CREATE TABLE t (id int);\n-- appended\n")
    );
}

#[test]
fn option_letter_shortcut_helper_drops_typed_char() {
    let ctx = egui::Context::default();
    let shortcut = egui::KeyboardShortcut::new(Modifiers::ALT, Key::X);
    let input = RawInput {
        events: vec![
            Event::Key {
                key: Key::X,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::ALT,
            },
            Event::Text("≈".into()),
        ],
        ..Default::default()
    };
    let mut found = false;
    let mut remaining = 0;
    ctx.run_ui(input, |ui| {
        found = consume_shortcut_with_text(ui.ctx(), &shortcut);
        remaining = ui.input(|i| i.events.len());
    })
    .drop_without_applying_deltas();
    assert!(found);
    assert_eq!(remaining, 0);
}

#[test]
fn ctrl_tab_moves_focus_out_of_the_editor() {
    let ctx = egui::Context::default();
    let lang = BasicSqlLanguage::new();
    let options = EditorOptions::default();
    let theme = EditorTheme::dark();
    let mut state = EditorState::new("focus", "select 1".to_owned());
    let mut other = String::new();
    let mut other_id = None;
    let mut run = |events: Vec<Event>, state: &mut EditorState, other: &mut String| {
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(600.0, 400.0))),
            events,
            ..Default::default()
        };
        let out = ctx.run_ui(input, |ui| {
            other_id = Some(ui.text_edit_singleline(other).id);
            let _ = SqlEditor::new(state, &lang, &options, &theme).show(ui);
        });
        out.drop_without_applying_deltas();
    };
    run(vec![], &mut state, &mut other);
    state.request_focus(&ctx);
    run(vec![], &mut state, &mut other);
    run(vec![], &mut state, &mut other);
    assert!(state.has_focus(&ctx));
    // Plain Tab indents and keeps the focus.
    let tab = |m: Modifiers| Event::Key {
        key: Key::Tab,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: m,
    };
    run(vec![tab(Modifiers::NONE)], &mut state, &mut other);
    assert!(state.has_focus(&ctx));
    assert_eq!(state.text(), "    select 1");
    // Ctrl+Shift+Tab moves to the previous widget.
    run(
        vec![
            Event::ModifiersChanged(Modifiers::CTRL.plus(Modifiers::SHIFT)),
            tab(Modifiers::CTRL.plus(Modifiers::SHIFT)),
        ],
        &mut state,
        &mut other,
    );
    run(vec![], &mut state, &mut other);
    assert!(!state.has_focus(&ctx));
    assert_eq!(ctx.memory(|m| m.focused()), other_id);
    assert_eq!(state.text(), "    select 1");
}

#[test]
fn plain_language_trait_defaults() {
    let lang = dbiji_editor::PlainText;
    assert!(lang.highlight("select").is_empty());
    assert_eq!(lang.line_comment(), "--");
    assert!(lang.complete("x", 1).is_none());
    assert!(!lang.should_auto_complete("x.", 2, '.'));
}

#[test]
fn folding_and_occurrences() {
    let text = "select id,\n  name\nfrom users;\n/* a\n   b */\nselect 1;\nselect total\nfrom orders\nwhere total > 1;\n";
    let mut h = Harness::new(text);
    h.focus();
    h.frame(vec![]);
    let regions = h.state.fold_regions();
    assert_eq!(
        regions,
        vec![(0, 2), (3, 4), (6, 8)],
        "statements and the block comment"
    );
    // Collapse all: only the first lines of the regions stay visible.
    h.state.fold_all();
    h.frame(vec![]);
    for l in [1, 2, 4, 7, 8] {
        assert!(h.state.is_line_hidden(l), "line {l} hidden");
    }
    for l in [0, 3, 5, 6] {
        assert!(!h.state.is_line_hidden(l), "line {l} visible");
    }
    // Moving down from line 0 skips the hidden lines.
    h.state.set_cursor(&h.ctx, 0);
    h.frame(vec![]);
    h.key(Key::ArrowDown, Modifiers::NONE);
    assert_eq!(
        h.state.line_col().0,
        4,
        "the cursor jumps over the fold (1-based line 4)"
    );
    // Moving the cursor into a fold opens it.
    let inside = h.state.line_range(7).start;
    h.state.set_cursor(&h.ctx, inside);
    h.frame(vec![]);
    h.frame(vec![]);
    assert!(!h.state.is_line_hidden(7), "the fold with the cursor opens");
    assert!(h.state.is_line_hidden(1), "other folds stay closed");
    // Editing a fold's first line opens it; the folds below shift with the new lines.
    h.state.set_cursor(&h.ctx, 0);
    h.frame(vec![]);
    h.state.insert_at_cursor(&h.ctx, "-- top\n\n");
    h.frame(vec![]);
    assert_eq!(
        h.state.folded_lines(),
        vec![5],
        "the comment fold moved from line 3 to 5"
    );
    assert!(h.state.is_line_hidden(6) && !h.state.is_line_hidden(3));
    h.state.unfold_all();
    h.frame(vec![]);
    assert!((0..h.state.line_count()).all(|l| !h.state.is_line_hidden(l)));
    // Toggle at the cursor.
    let l = h.state.line_range(3).start;
    h.state.set_cursor(&h.ctx, l);
    assert!(h.state.toggle_fold_at_cursor());
    h.frame(vec![]);
    assert!(h.state.is_line_hidden(3) || h.state.is_line_hidden(4));
    h.state.unfold_all();
    h.frame(vec![]);
}
