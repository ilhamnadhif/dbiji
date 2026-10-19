//! Editor behaviour options and key bindings.

use egui::{FontFamily, Key, KeyboardShortcut, Modifiers};

/// Behaviour and appearance options of [`crate::SqlEditor`].
///
/// Serializable so the application can persist them with its settings (the
/// key bindings and font family are not serialized; they come from
/// [`KeyBindings::default`] and the monospace family).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct EditorOptions {
    /// Font size in points. Use it for editor zoom.
    pub font_size: f32,
    /// Font family (default: monospace).
    #[serde(skip, default = "default_family")]
    pub font_family: FontFamily,
    /// Extra vertical space between rows, in points.
    pub line_spacing: f32,
    /// Width of an indentation level in columns.
    pub tab_size: usize,
    /// Insert spaces instead of `\t` when pressing Tab.
    pub insert_spaces: bool,
    /// Show the line-number gutter.
    pub line_numbers: bool,
    /// Fold markers in the gutter; collapsed regions hide their lines.
    pub folding: bool,
    /// Highlight the other occurrences of the word under the cursor.
    pub mark_occurrences: bool,
    /// Highlight the row(s) of the cursor line.
    pub highlight_current_line: bool,
    /// Highlight the bracket matching the one next to the cursor.
    pub highlight_matching_brackets: bool,
    /// Soft-wrap long lines at the editor width.
    pub word_wrap: bool,
    /// Insert closing `)` `]` `}` `'` `"` `` ` `` automatically and type over them.
    pub auto_close_brackets: bool,
    /// Open the completion popup automatically after trigger characters
    /// such as `.` ([`crate::EditorLanguage::should_auto_complete`]). The
    /// manual trigger (Ctrl+Space, [`crate::EditorState::trigger_completion`])
    /// works regardless.
    pub auto_complete: bool,
    /// Delay before the popup opens automatically while typing letters.
    pub auto_complete_delay_ms: u64,
    /// Open the popup automatically while typing identifier characters
    /// (otherwise only after trigger characters such as `.`).
    pub auto_complete_on_letters: bool,
    /// Minimum identifier length before letters trigger the popup.
    pub auto_complete_min_chars: usize,
    /// Disallow editing (selection, copy, find and navigation still work).
    pub read_only: bool,
    /// Blink the text cursor (each blink costs a repaint; `false` keeps the
    /// editor fully idle).
    pub cursor_blink: bool,
    /// Show the built-in context menu on right click.
    pub context_menu: bool,
    /// Copy / cut the whole line when nothing is selected.
    pub copy_line_without_selection: bool,
    /// Documents larger than this (bytes) are shown without highlighting.
    pub max_highlight_bytes: usize,
    /// Documents up to this size (bytes) are re-highlighted synchronously on
    /// every edit; larger ones get a cheap per-line update while typing and a
    /// full re-highlight after [`EditorOptions::highlight_debounce_ms`].
    pub sync_highlight_bytes: usize,
    /// Debounce of the full re-highlight for large documents.
    pub highlight_debounce_ms: u64,
    /// Maximum number of undo steps kept.
    pub undo_limit: usize,
    /// Key bindings of editor commands.
    #[serde(skip)]
    pub keys: KeyBindings,
}

fn default_family() -> FontFamily {
    FontFamily::Monospace
}

impl Default for EditorOptions {
    fn default() -> Self {
        Self {
            font_size: 13.0,
            font_family: FontFamily::Monospace,
            line_spacing: 3.0,
            tab_size: 4,
            insert_spaces: true,
            line_numbers: true,
            folding: true,
            mark_occurrences: true,
            highlight_current_line: true,
            highlight_matching_brackets: true,
            word_wrap: false,
            auto_close_brackets: true,
            auto_complete: true,
            auto_complete_delay_ms: 300,
            auto_complete_on_letters: false,
            auto_complete_min_chars: 2,
            read_only: false,
            cursor_blink: true,
            context_menu: true,
            copy_line_without_selection: true,
            max_highlight_bytes: 2 * 1024 * 1024,
            sync_highlight_bytes: 256 * 1024,
            highlight_debounce_ms: 250,
            undo_limit: 1000,
            keys: KeyBindings::default(),
        }
    }
}

impl EditorOptions {
    /// Options for a read-only viewer ([`crate::code_view`]).
    pub fn read_only_viewer() -> Self {
        Self {
            read_only: true,
            highlight_current_line: false,
            auto_complete: false,
            context_menu: true,
            cursor_blink: false,
            ..Self::default()
        }
    }

    /// One indentation level (`tab_size` spaces or a tab).
    pub fn indent_unit(&self) -> String {
        if self.insert_spaces {
            " ".repeat(self.tab_size.max(1))
        } else {
            "\t".to_owned()
        }
    }
}

/// Key bindings of editor commands. Each command accepts any of its
/// shortcuts; an empty list disables the command's shortcut.
///
/// Navigation, selection and clipboard keys follow the platform conventions
/// and are not configurable here. Application-level shortcuts (execute,
/// format, ...) are not handled by the editor and stay in egui's input for
/// the application.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyBindings {
    /// Undo.
    pub undo: Vec<KeyboardShortcut>,
    /// Redo.
    pub redo: Vec<KeyboardShortcut>,
    /// Select all.
    pub select_all: Vec<KeyboardShortcut>,
    /// Toggle line comment.
    pub toggle_comment: Vec<KeyboardShortcut>,
    /// Toggle block comment.
    pub toggle_block_comment: Vec<KeyboardShortcut>,
    /// Duplicate line or selection.
    pub duplicate: Vec<KeyboardShortcut>,
    /// Delete the current / selected lines.
    pub delete_line: Vec<KeyboardShortcut>,
    /// Move lines up.
    pub move_line_up: Vec<KeyboardShortcut>,
    /// Move lines down.
    pub move_line_down: Vec<KeyboardShortcut>,
    /// Upper-case selection / word.
    pub upper_case: Vec<KeyboardShortcut>,
    /// Lower-case selection / word.
    pub lower_case: Vec<KeyboardShortcut>,
    /// Open the completion popup.
    pub trigger_completion: Vec<KeyboardShortcut>,
    /// Open the find bar.
    pub find: Vec<KeyboardShortcut>,
    /// Open the find bar with the replace row.
    pub replace: Vec<KeyboardShortcut>,
    /// Next find match.
    pub find_next: Vec<KeyboardShortcut>,
    /// Previous find match.
    pub find_previous: Vec<KeyboardShortcut>,
    /// Open the go-to-line popup.
    pub goto_line: Vec<KeyboardShortcut>,
    /// Select the word at the cursor.
    pub select_word: Vec<KeyboardShortcut>,
    /// Select the current line(s).
    pub select_line: Vec<KeyboardShortcut>,
    /// Emit [`crate::EditorEvent::SaveRequested`].
    pub save: Vec<KeyboardShortcut>,
}

const fn sc(modifiers: Modifiers, key: Key) -> KeyboardShortcut {
    KeyboardShortcut::new(modifiers, key)
}

const CMD: Modifiers = Modifiers::COMMAND;
const CMD_SHIFT: Modifiers = Modifiers::COMMAND.plus(Modifiers::SHIFT);
const CMD_ALT: Modifiers = Modifiers::COMMAND.plus(Modifiers::ALT);

impl Default for KeyBindings {
    fn default() -> Self {
        let mac = cfg!(target_os = "macos");
        Self {
            undo: vec![sc(CMD, Key::Z)],
            redo: if mac {
                vec![sc(CMD_SHIFT, Key::Z)]
            } else {
                vec![sc(CMD, Key::Y), sc(CMD_SHIFT, Key::Z)]
            },
            select_all: vec![sc(CMD, Key::A)],
            toggle_comment: vec![sc(CMD, Key::Slash)],
            toggle_block_comment: vec![sc(CMD_SHIFT, Key::Slash)],
            duplicate: vec![sc(CMD, Key::D)],
            delete_line: vec![sc(CMD_SHIFT, Key::K)],
            move_line_up: vec![sc(Modifiers::ALT, Key::ArrowUp)],
            move_line_down: vec![sc(Modifiers::ALT, Key::ArrowDown)],
            upper_case: vec![sc(CMD_SHIFT, Key::X)],
            lower_case: vec![sc(CMD_SHIFT, Key::Y)],
            trigger_completion: vec![sc(Modifiers::CTRL, Key::Space)],
            find: vec![sc(CMD, Key::F)],
            replace: if mac {
                vec![sc(CMD_ALT, Key::F)]
            } else {
                vec![sc(CMD, Key::H), sc(CMD_ALT, Key::F)]
            },
            find_next: vec![sc(CMD, Key::G), sc(Modifiers::NONE, Key::F3)],
            find_previous: vec![sc(CMD_SHIFT, Key::G), sc(Modifiers::SHIFT, Key::F3)],
            goto_line: vec![sc(CMD, Key::L)],
            select_word: Vec::new(),
            select_line: vec![sc(CMD_SHIFT, Key::L)],
            save: vec![sc(CMD, Key::S)],
        }
    }
}

/// Does a key event (`key`, `modifiers`) trigger `shortcut`?
///
/// Shift and Alt must match exactly; Cmd/Ctrl follow egui's platform rules.
/// `?` with Shift is accepted for a `Shift+/` binding (US layouts report the
/// shifted character as the logical key).
pub(crate) fn shortcut_matches(
    shortcut: &KeyboardShortcut,
    key: Key,
    modifiers: Modifiers,
) -> bool {
    let key_ok = key == shortcut.logical_key
        || (shortcut.logical_key == Key::Slash
            && shortcut.modifiers.shift
            && key == Key::Questionmark);
    key_ok && modifiers.matches_exact(shortcut.modifiers)
}

pub(crate) fn any_matches(shortcuts: &[KeyboardShortcut], key: Key, modifiers: Modifiers) -> bool {
    shortcuts
        .iter()
        .any(|s| shortcut_matches(s, key, modifiers))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_is_exact_for_shift_and_alt() {
        let undo = sc(CMD, Key::Z);
        let redo = sc(CMD_SHIFT, Key::Z);
        let cmd = if cfg!(target_os = "macos") {
            Modifiers::MAC_CMD.plus(Modifiers::COMMAND)
        } else {
            Modifiers::CTRL.plus(Modifiers::COMMAND)
        };
        assert!(shortcut_matches(&undo, Key::Z, cmd));
        assert!(!shortcut_matches(&undo, Key::Z, cmd.plus(Modifiers::SHIFT)));
        assert!(shortcut_matches(&redo, Key::Z, cmd.plus(Modifiers::SHIFT)));
        assert!(!shortcut_matches(&redo, Key::Z, cmd));
        let block = sc(CMD_SHIFT, Key::Slash);
        assert!(shortcut_matches(
            &block,
            Key::Questionmark,
            cmd.plus(Modifiers::SHIFT)
        ));
        assert!(!shortcut_matches(
            &sc(CMD, Key::Slash),
            Key::Questionmark,
            cmd.plus(Modifiers::SHIFT)
        ));
        let completion = sc(Modifiers::CTRL, Key::Space);
        assert!(shortcut_matches(
            &completion,
            Key::Space,
            Modifiers::CTRL.plus(if cfg!(target_os = "macos") {
                Modifiers::NONE
            } else {
                Modifiers::COMMAND
            })
        ));
        assert!(!shortcut_matches(&completion, Key::Space, Modifiers::NONE));
    }

    #[test]
    fn options_roundtrip_through_json() {
        let o = EditorOptions {
            font_size: 15.0,
            word_wrap: true,
            ..EditorOptions::default()
        };
        let json = serde_json::to_string(&o).expect("serialize");
        let back: EditorOptions = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, o);
        let partial: EditorOptions = serde_json::from_str(r#"{"tab_size": 2}"#).expect("partial");
        assert_eq!(partial.tab_size, 2);
        assert_eq!(partial.font_family, FontFamily::Monospace);
        assert_eq!(EditorOptions::default().indent_unit(), "    ");
    }
}
