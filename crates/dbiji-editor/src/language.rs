//! The language integration point of the editor.
//!
//! The editor itself is language-agnostic: highlighting, completion, hover
//! documentation, object hyperlinks and spell suggestions all come through
//! [`EditorLanguage`], which the application implements (DBiji adapts its
//! `dbiji-sql` tokenizer and completion engine to it). All offsets exchanged
//! through this trait are **byte offsets** into the document text.

use std::ops::Range;

/// Visual class of a token produced by [`EditorLanguage::highlight`].
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
pub enum TokenStyle {
    /// Reserved or non-reserved keyword (`SELECT`, `FROM`, `ORDER BY` ...).
    Keyword,
    /// Unquoted identifier (table, column, alias ...).
    Identifier,
    /// Quoted identifier (`"Name"`, `` `name` ``, `[name]`).
    QuotedIdentifier,
    /// String literal (`'text'`, `E'..'`, `$$..$$`).
    String,
    /// Numeric literal.
    Number,
    /// Line or block comment.
    Comment,
    /// Operator (`=`, `<>`, `||`, `::` ...).
    Operator,
    /// Punctuation (`(`, `)`, `,`, `;`, `.`).
    Punctuation,
    /// Bind parameter (`?`, `$1`, `:name`, `${name}`).
    Parameter,
    /// Variable (`@var`, `@@session`).
    Variable,
    /// Built-in or user function name.
    Function,
    /// Data type name (`INTEGER`, `varchar`, `jsonb` ...).
    DataType,
    /// Client side command (`@set`, `@include`, `DELIMITER`).
    ClientCommand,
    /// Anything else (rendered with the plain text color).
    #[default]
    Plain,
}

impl TokenStyle {
    /// All styles, in declaration order.
    pub const ALL: [TokenStyle; 14] = [
        TokenStyle::Keyword,
        TokenStyle::Identifier,
        TokenStyle::QuotedIdentifier,
        TokenStyle::String,
        TokenStyle::Number,
        TokenStyle::Comment,
        TokenStyle::Operator,
        TokenStyle::Punctuation,
        TokenStyle::Parameter,
        TokenStyle::Variable,
        TokenStyle::Function,
        TokenStyle::DataType,
        TokenStyle::ClientCommand,
        TokenStyle::Plain,
    ];

    /// `true` for styles whose content is not code (strings, comments and
    /// quoted identifiers): brackets inside them are not matched and quotes
    /// and brackets typed inside them are not auto-closed.
    pub fn is_literal(self) -> bool {
        matches!(
            self,
            TokenStyle::String | TokenStyle::Comment | TokenStyle::QuotedIdentifier
        )
    }
}

/// Kind of a completion proposal; selects the icon shown in the popup.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
pub enum EntryKind {
    /// SQL keyword.
    #[default]
    Keyword,
    /// Database / catalog.
    Database,
    /// Schema.
    Schema,
    /// Table.
    Table,
    /// View or materialized view.
    View,
    /// Column.
    Column,
    /// Function.
    Function,
    /// Stored procedure.
    Procedure,
    /// Sequence.
    Sequence,
    /// Index.
    Index,
    /// Trigger.
    Trigger,
    /// Data type.
    DataType,
    /// Table alias or common table expression name.
    Alias,
    /// Variable or parameter.
    Variable,
    /// Code template / snippet.
    Snippet,
    /// Anything else.
    Other,
}

impl EntryKind {
    /// Phosphor icon glyph for this kind (requires the app to install the
    /// egui-phosphor font, see `egui_phosphor::add_to_fonts`).
    pub fn icon(self) -> &'static str {
        use egui_phosphor::regular as ph;
        match self {
            EntryKind::Keyword => ph::TEXT_AA,
            EntryKind::Database => ph::DATABASE,
            EntryKind::Schema => ph::TREE_STRUCTURE,
            EntryKind::Table => ph::TABLE,
            EntryKind::View => ph::EYE,
            EntryKind::Column => ph::COLUMNS,
            EntryKind::Function => ph::FUNCTION,
            EntryKind::Procedure => ph::GEAR_SIX,
            EntryKind::Sequence => ph::LIST_NUMBERS,
            EntryKind::Index => ph::LIGHTNING,
            EntryKind::Trigger => ph::FLOW_ARROW,
            EntryKind::DataType => ph::CUBE,
            EntryKind::Alias => ph::TAG,
            EntryKind::Variable => ph::AT,
            EntryKind::Snippet => ph::CODE_BLOCK,
            EntryKind::Other => ph::DOT_OUTLINE,
        }
    }
}

/// One completion proposal.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct CompletionEntry {
    /// Text shown in the popup; also what typed characters are matched against.
    pub label: String,
    /// Text inserted over [`CompletionList::replace`] when the entry is accepted.
    pub insert: String,
    /// Icon / category.
    pub kind: EntryKind,
    /// Optional secondary text shown dimmed (type, schema, signature ...).
    pub detail: Option<String>,
    /// After inserting, move the cursor this many **characters** back from
    /// the end of the inserted text (e.g. `1` for `count()` to land between
    /// the parentheses).
    pub cursor_back: usize,
}

impl CompletionEntry {
    /// Entry inserting its own label.
    pub fn new(label: impl Into<String>, kind: EntryKind) -> Self {
        let label = label.into();
        Self {
            insert: label.clone(),
            label,
            kind,
            detail: None,
            cursor_back: 0,
        }
    }

    /// Builder: set the inserted text.
    pub fn with_insert(mut self, insert: impl Into<String>) -> Self {
        self.insert = insert.into();
        self
    }

    /// Builder: set the detail text.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Builder: set [`CompletionEntry::cursor_back`].
    pub fn with_cursor_back(mut self, chars: usize) -> Self {
        self.cursor_back = chars;
        self
    }
}

/// Result of [`EditorLanguage::complete`].
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct CompletionList {
    /// Byte range of the text replaced when an entry is accepted (usually the
    /// partial identifier before the cursor). Must contain the cursor offset
    /// or end at it.
    pub replace: Range<usize>,
    /// Proposals, in the provider's order of relevance. The editor keeps this
    /// order while filtering, ranking prefix matches first.
    pub items: Vec<CompletionEntry>,
}

/// Language services used by the editor. Implemented by the application.
///
/// Every method receives the full document text and byte offsets into it.
/// Methods are called on the UI thread: [`EditorLanguage::highlight`] runs
/// after edits (synchronously for small documents, debounced for large
/// ones), [`EditorLanguage::complete`] when completion is triggered,
/// [`EditorLanguage::hover`] once the pointer rests on a word,
/// [`EditorLanguage::link_at`] while the platform command key is held over
/// the text, and [`EditorLanguage::spell_suggestions`] when a context menu
/// opens on a misspelled word.
pub trait EditorLanguage {
    /// Token spans (byte ranges, any order, overlaps are resolved in favour
    /// of the earlier span) for the whole `text`. Unstyled gaps are
    /// [`TokenStyle::Plain`].
    fn highlight(&self, text: &str) -> Vec<(Range<usize>, TokenStyle)>;

    /// Completion proposals at byte offset `cursor`, or `None`.
    fn complete(&self, text: &str, cursor: usize) -> Option<CompletionList>;

    /// Should typing `typed` (just inserted before `cursor`) open the
    /// completion popup immediately? Default: after `.`.
    fn should_auto_complete(&self, text: &str, cursor: usize, typed: char) -> bool {
        let _ = (text, cursor);
        typed == '.'
    }

    /// Hover documentation for the token at `offset` (plain text, may contain
    /// newlines).
    fn hover(&self, text: &str, offset: usize) -> Option<String> {
        let _ = (text, offset);
        None
    }

    /// Byte range of a navigable object name at `offset` (FR-S4 hyperlinks).
    /// While the command key is held the range is underlined; clicking it
    /// emits [`crate::EditorEvent::LinkClicked`].
    fn link_at(&self, text: &str, offset: usize) -> Option<Range<usize>> {
        let _ = (text, offset);
        None
    }

    /// Prefix used by "toggle line comment".
    fn line_comment(&self) -> &str {
        "--"
    }

    /// Delimiters used by "toggle block comment", if the language has them.
    fn block_comment(&self) -> Option<(&str, &str)> {
        Some(("/*", "*/"))
    }

    /// Spelling suggestions for a word flagged by
    /// [`crate::EditorState::set_spell_issues`].
    fn spell_suggestions(&self, word: &str) -> Vec<String> {
        let _ = word;
        Vec::new()
    }

    /// Identifier character class (word navigation, double-click selection,
    /// whole-word search, completion prefixes).
    fn is_word_char(&self, c: char) -> bool {
        crate::text::is_word_char(c)
    }

    /// Bump this value whenever highlighting rules change for the same text
    /// (dialect switched, client commands toggled, ...) to force a re-highlight.
    fn highlight_generation(&self) -> u64 {
        0
    }

    /// Foldable byte ranges (statements, block comments) spanning several lines.
    fn fold_ranges(&self, text: &str) -> Vec<Range<usize>> {
        let _ = text;
        Vec::new()
    }
}

/// A language with no highlighting and no completion (plain text editing).
#[derive(Clone, Copy, Debug, Default)]
pub struct PlainText;

impl EditorLanguage for PlainText {
    fn highlight(&self, _text: &str) -> Vec<(Range<usize>, TokenStyle)> {
        Vec::new()
    }

    fn complete(&self, _text: &str, _cursor: usize) -> Option<CompletionList> {
        None
    }

    fn should_auto_complete(&self, _text: &str, _cursor: usize, _typed: char) -> bool {
        false
    }

    fn block_comment(&self) -> Option<(&str, &str)> {
        None
    }
}
