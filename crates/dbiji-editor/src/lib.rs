//! SQL code editor widget for DBiji (egui).
//!
//! The editor is language-agnostic: highlighting, completion, hover text,
//! object hyperlinks and spell suggestions come from an [`EditorLanguage`]
//! implementation supplied by the application (DBiji adapts `dbiji-sql`).
//! [`BasicSqlLanguage`] is a small built-in implementation used by the demo
//! and tests.
//!
//! # Usage
//!
//! ```no_run
//! use dbiji_editor::{BasicSqlLanguage, EditorEvent, EditorOptions, EditorState, EditorTheme, SqlEditor};
//!
//! struct Tab {
//!     editor: EditorState,
//! }
//!
//! fn show(ui: &mut egui::Ui, tab: &mut Tab, lang: &BasicSqlLanguage, options: &EditorOptions) {
//!     let theme = EditorTheme::from_visuals(ui.visuals());
//!     let out = SqlEditor::new(&mut tab.editor, lang, options, &theme).show(ui);
//!     if out.changed {
//!         // re-run statement splitting / diagnostics for `tab.editor.text()`
//!     }
//!     for event in out.events {
//!         if let EditorEvent::LinkClicked { text, .. } = event {
//!             println!("open object editor for {text}");
//!         }
//!     }
//! }
//! ```
//!
//! # Design
//!
//! The widget is built on egui's text building blocks (epaint layout and
//! galleys, `CCursor` navigation, IME events, `IMEOutput`, AccessKit node)
//! rather than on `egui::TextEdit`, so that it can be *virtualized*: only
//! visible lines own a laid-out galley, every edit costs time proportional
//! to the edited lines, and memory is proportional to the visible text. It
//! keeps its own operation-based undo history, processes all keyboard and
//! IME events in order, and paints selections without losing syntax colors.
//!
//! All offsets in the public API are **byte offsets** into the document
//! text; columns reported by [`EditorState::line_col`] count characters.

#![warn(missing_docs)]

mod code_view;
mod completion;
mod find;
mod find_bar;
mod input;
mod language;
mod layout;
mod ops;
mod options;
mod sql;
mod state;
mod text;
mod theme;
mod undo;
mod widget;

pub use code_view::{CodeView, code_view};
pub use find::FindOptions;
pub use language::{
    CompletionEntry, CompletionList, EditorLanguage, EntryKind, PlainText, TokenStyle,
};
pub use options::{EditorOptions, KeyBindings};
pub use sql::{BasicSqlLanguage, TableInfo};
pub use state::{Diagnostic, EditorState, Severity, ViewState};
pub use text::LineEnding;
pub use theme::EditorTheme;
pub use widget::{ContextAction, EditorEvent, EditorResponse, SqlEditor};

/// Every user-visible English string of this crate (all of them go through
/// `dbiji_core::tr` / `trf`). Used to check locale coverage.
pub const TRANSLATABLE_STRINGS: &[&str] = &[
    "Find",
    "Replace",
    "Replace all",
    "Toggle replace",
    "Match case",
    "Whole word",
    "Use regular expression",
    "Invalid regular expression: {}",
    "No results",
    "{} of {}",
    "{} matches",
    "Previous match",
    "Next match",
    "Close",
    "Go to line (1 - {}):",
    "Go to line",
    "Execute statement",
    "Execute script",
    "Execute in new tab",
    "Explain execution plan",
    "Format SQL",
    "Cut",
    "Copy",
    "Paste",
    "Select all",
    "Undo",
    "Redo",
    "Toggle comment",
    "Toggle block comment",
    "Upper case",
    "Lower case",
    "Add \"{}\" to dictionary",
    "No suggestions",
    "Error",
    "Warning",
    "Info",
    "{} columns",
    "Table {}",
    "Function {}",
];

/// Consume `shortcut` from egui's input together with the text it typed.
///
/// On macOS, Option+letter shortcuts (e.g. ⌥X "execute script") also emit
/// the typed character (`≈`). Call this *before* showing the editor for
/// application shortcuts that use Alt/Option so the character is not
/// inserted. Returns `true` if the shortcut was pressed.
pub fn consume_shortcut_with_text(ctx: &egui::Context, shortcut: &egui::KeyboardShortcut) -> bool {
    ctx.input_mut(|i| {
        let mut found = false;
        let mut drop_text = false;
        i.events.retain(|e| match e {
            egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } if *key == shortcut.logical_key && modifiers.matches_exact(shortcut.modifiers) => {
                found = true;
                drop_text = true;
                false
            }
            egui::Event::Text(_) if drop_text => {
                drop_text = false;
                false
            }
            _ => true,
        });
        found
    })
}
