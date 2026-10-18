//! Randomized robustness test: long random sequences of real input events
//! (unicode typing, IME, paste, editing commands, navigation, completion,
//! find, undo/redo, mouse clicks) must never panic, must keep the selection
//! on char boundaries, and undoing everything must restore the original text.

use dbiji_editor::{
    BasicSqlLanguage, Diagnostic, EditorOptions, EditorState, EditorTheme, Severity, SqlEditor,
    TableInfo,
};
use egui::{Event, ImeEvent, Key, Modifiers, PointerButton, Pos2, RawInput, Rect, vec2};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn cmd() -> Modifiers {
    if cfg!(target_os = "macos") {
        Modifiers::MAC_CMD.plus(Modifiers::COMMAND)
    } else {
        Modifiers::CTRL.plus(Modifiers::COMMAND)
    }
}

fn key(k: Key, m: Modifiers) -> Vec<Event> {
    vec![
        Event::ModifiersChanged(m),
        Event::Key {
            key: k,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: m,
        },
        Event::Key {
            key: k,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: m,
        },
        Event::ModifiersChanged(Modifiers::NONE),
    ]
}

const SNIPPETS: &[&str] = &[
    "a", "é", "👍", "中", " ", "(", ")", "'", "\"", "`", "[", "]", ".", ";", "SELECT ", "u.",
    "/* ", "*/", "-- c", "x\u{301}",
];

fn random_events(rng: &mut Rng, word: Modifiers) -> Vec<Event> {
    // Insertions dominate so documents grow over a run.
    let r = rng.below(60);
    if r >= 40 {
        return match r {
            40..=55 => vec![Event::Text(SNIPPETS[rng.below(SNIPPETS.len())].to_owned())],
            56..=57 => key(Key::Enter, Modifiers::NONE),
            _ => vec![Event::Paste("SELECT a, b\nFROM t -- é\n".to_owned())],
        };
    }
    match r {
        0..=11 => vec![Event::Text(SNIPPETS[rng.below(SNIPPETS.len())].to_owned())],
        12 => key(Key::Enter, Modifiers::NONE),
        13 => key(Key::Backspace, Modifiers::NONE),
        14 => key(Key::Delete, Modifiers::NONE),
        15 => key(Key::Backspace, word),
        16 => key(Key::Tab, Modifiers::NONE),
        17 => key(Key::Tab, Modifiers::SHIFT),
        18 => key(Key::Slash, cmd()),
        19 => key(Key::D, cmd()),
        20 => key(Key::K, cmd().plus(Modifiers::SHIFT)),
        21 => key(Key::ArrowUp, Modifiers::ALT),
        22 => key(Key::ArrowDown, Modifiers::ALT),
        23 => key(Key::X, cmd().plus(Modifiers::SHIFT)),
        24 => key(Key::Z, cmd()),
        25 => key(Key::Z, cmd().plus(Modifiers::SHIFT)),
        26 => {
            let k = [
                Key::ArrowLeft,
                Key::ArrowRight,
                Key::ArrowUp,
                Key::ArrowDown,
                Key::Home,
                Key::End,
                Key::PageUp,
                Key::PageDown,
            ][rng.below(8)];
            let m = [
                Modifiers::NONE,
                Modifiers::SHIFT,
                word,
                word.plus(Modifiers::SHIFT),
            ][rng.below(4)];
            key(k, m)
        }
        27 => vec![Event::Paste(
            ["x\ny", "é\n\n👍", "SELECT 1;\r\nSELECT 2;", ""][rng.below(4)].to_owned(),
        )],
        28 => vec![Event::Cut],
        29 => vec![Event::Copy],
        30 => vec![Event::Ime(ImeEvent::Preedit {
            text: ["n", "ni", "你", ""][rng.below(4)].to_owned(),
            active_range_chars: None,
        })],
        31 => vec![Event::Ime(ImeEvent::Commit(
            ["你好", "é", "("][rng.below(3)].to_owned(),
        ))],
        32 => key(Key::Space, Modifiers::CTRL),
        33 => key(Key::Escape, Modifiers::NONE),
        34 if rng.below(8) == 0 => key(Key::A, cmd()),
        34 => key(Key::E, Modifiers::CTRL),
        35 => key(Key::Slash, cmd().plus(Modifiers::SHIFT)),
        36 => key(Key::G, cmd()),
        37 => {
            let pos = Pos2::new(20.0 + rng.below(500) as f32, 5.0 + rng.below(300) as f32);
            vec![
                Event::PointerMoved(pos),
                Event::PointerButton {
                    pos,
                    button: PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
                Event::PointerButton {
                    pos,
                    button: PointerButton::Primary,
                    pressed: false,
                    modifiers: Modifiers::NONE,
                },
            ]
        }
        38 => key(Key::Y, cmd().plus(Modifiers::SHIFT)),
        _ => key(Key::Enter, Modifiers::NONE),
    }
}

fn check(state: &EditorState) {
    let text = state.text();
    let (anchor, cursor) = state.selection_ends();
    assert!(
        anchor <= text.len() && cursor <= text.len(),
        "selection out of range"
    );
    assert!(
        text.is_char_boundary(anchor) && text.is_char_boundary(cursor),
        "selection not on char boundary"
    );
    assert_eq!(state.line_count(), text.matches('\n').count() + 1);
    for d in state.diagnostics() {
        assert!(d.range.start <= d.range.end && d.range.end <= text.len());
        assert!(text.is_char_boundary(d.range.start) && text.is_char_boundary(d.range.end));
    }
    let (line, col) = state.line_col();
    assert!(line >= 1 && col >= 1);
}

fn run(seed: u64, steps: usize, word_wrap: bool, sync_highlight: bool) {
    let initial = "SELECT u.id, u.name -- é\nFROM users u\n\nWHERE u.id = 1;\n/* x */ SELECT 👍;\n";
    let ctx = egui::Context::default();
    let lang = BasicSqlLanguage::new().with_tables(vec![TableInfo {
        name: "users".into(),
        columns: vec!["id".into(), "name".into()],
    }]);
    let options = EditorOptions {
        cursor_blink: false,
        word_wrap,
        auto_complete_on_letters: true,
        auto_complete_delay_ms: 0,
        sync_highlight_bytes: if sync_highlight { 1 << 20 } else { 0 },
        highlight_debounce_ms: 30,
        ..EditorOptions::default()
    };
    let theme = EditorTheme::dark();
    let mut state = EditorState::new(("monkey", seed), initial.to_owned());
    state.set_diagnostics(vec![
        Diagnostic::new(7..11, Severity::Error, "e"),
        Diagnostic::new(30..30, Severity::Info, "i"),
    ]);
    state.set_spell_issues(vec![0..6, 40..44]);
    state.set_statement_highlight(Some(0..20));
    let mut rng = Rng(seed);
    let word = if cfg!(target_os = "macos") {
        Modifiers::ALT
    } else {
        Modifiers::CTRL
    };
    let mut time = 0.0;
    let frame = |events: Vec<Event>, state: &mut EditorState, time: &mut f64| {
        *time += 1.0 / 60.0;
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(640.0, 360.0))),
            time: Some(*time),
            focused: true,
            events,
            ..Default::default()
        };
        let out = ctx.run_ui(input, |ui| {
            let _ = SqlEditor::new(state, &lang, &options, &theme).show(ui);
        });
        out.drop_without_applying_deltas();
    };
    frame(vec![], &mut state, &mut time);
    state.request_focus(&ctx);
    frame(vec![], &mut state, &mut time);
    for step in 0..steps {
        if step % 97 == 0 {
            // Find bar round trip (typing goes into the find field).
            state.open_find(rng.below(2) == 0);
            frame(vec![], &mut state, &mut time);
            frame(vec![Event::Text("u".into())], &mut state, &mut time);
            frame(key(Key::Escape, Modifiers::NONE), &mut state, &mut time);
            frame(vec![], &mut state, &mut time);
        }
        let events = random_events(&mut rng, word);
        frame(events, &mut state, &mut time);
        check(&state);
        if !state.has_focus(&ctx) {
            state.request_focus(&ctx);
            frame(vec![], &mut state, &mut time);
        }
    }
    let edits = state.version();
    eprintln!(
        "seed {seed:x}: {edits} edits, {} bytes, {} lines",
        state.text().len(),
        state.line_count()
    );
    assert!(
        edits > steps as u64 / 4,
        "too few edits reached the editor: {edits}"
    );
    // Finish any composition, then undo everything.
    frame(
        vec![Event::Ime(ImeEvent::Commit(String::new()))],
        &mut state,
        &mut time,
    );
    let mut guard = 0;
    while state.can_undo() && guard < 10_000 {
        state.undo(&ctx);
        frame(vec![], &mut state, &mut time);
        check(&state);
        guard += 1;
    }
    assert_eq!(
        state.text(),
        initial,
        "undoing everything must restore the original (seed {seed})"
    );
    while state.can_redo() && guard < 20_000 {
        state.redo(&ctx);
        guard += 1;
    }
    frame(vec![], &mut state, &mut time);
    check(&state);
}

#[test]
fn monkey_no_wrap_sync_highlight() {
    for seed in 1u64..=4 {
        run(0x9E37_79B9_7F4A_7C15 ^ seed, 800, false, true);
    }
}

#[test]
fn monkey_word_wrap_deferred_highlight() {
    for seed in 11u64..=13 {
        run(0xD1B5_4A32_D192_ED03 ^ seed, 800, true, false);
    }
}

/// Long run over many seeds: `cargo test -p dbiji-editor --test monkey -- --ignored`.
#[test]
#[ignore]
fn monkey_long() {
    for seed in 100u64..140 {
        run(
            seed.wrapping_mul(0x2545_F491_4F6C_DD1D),
            1500,
            seed % 2 == 0,
            seed % 3 != 0,
        );
    }
}
