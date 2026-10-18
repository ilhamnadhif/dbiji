//! The SQL editor's language services (FR-S3..S6), implemented with
//! `dbiji-sql` for the editor widget: highlighting in the connection's
//! dialect, schema-aware autocomplete, hover help, object hyperlinks and
//! spelling suggestions.

use dbiji_core::Dialect;
use dbiji_core::dialect::SqlSyntax;
use dbiji_editor::{CompletionEntry, CompletionList, EditorLanguage, EntryKind, TokenStyle};
use dbiji_sql::{CompletionCatalog, CompletionKind, CompletionOptions, HighlightClass, SnapshotCatalog};
use std::cell::RefCell;
use std::ops::Range;
use std::sync::Arc;

pub struct SqlLanguage {
    pub dialect: Arc<dyn Dialect>,
    syntax: SqlSyntax,
    catalog: Option<Arc<SnapshotCatalog>>,
    pub completion: CompletionOptions,
    generation: u64,
    /// Highlight cache keyed by the text length and a cheap hash.
    cache: RefCell<Option<(u64, Vec<(Range<usize>, TokenStyle)>)>>,
}

fn style(c: HighlightClass) -> TokenStyle {
    match c {
        HighlightClass::Keyword => TokenStyle::Keyword,
        HighlightClass::Identifier => TokenStyle::Identifier,
        HighlightClass::QuotedIdentifier => TokenStyle::QuotedIdentifier,
        HighlightClass::String => TokenStyle::String,
        HighlightClass::Number => TokenStyle::Number,
        HighlightClass::Comment => TokenStyle::Comment,
        HighlightClass::Operator => TokenStyle::Operator,
        HighlightClass::Punctuation => TokenStyle::Punctuation,
        HighlightClass::Parameter => TokenStyle::Parameter,
        HighlightClass::Variable => TokenStyle::Variable,
        HighlightClass::Function => TokenStyle::Function,
        HighlightClass::DataType => TokenStyle::DataType,
        HighlightClass::ClientCommand => TokenStyle::ClientCommand,
        HighlightClass::Plain => TokenStyle::Plain,
    }
}

fn entry_kind(k: CompletionKind) -> EntryKind {
    match k {
        CompletionKind::Keyword => EntryKind::Keyword,
        CompletionKind::Schema => EntryKind::Schema,
        CompletionKind::Table => EntryKind::Table,
        CompletionKind::View => EntryKind::View,
        CompletionKind::Column => EntryKind::Column,
        CompletionKind::Alias => EntryKind::Alias,
        CompletionKind::Function => EntryKind::Function,
        CompletionKind::Cte => EntryKind::Table,
        CompletionKind::Join => EntryKind::Snippet,
        CompletionKind::Snippet => EntryKind::Snippet,
        CompletionKind::Variable => EntryKind::Variable,
    }
}

fn text_key(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

impl SqlLanguage {
    pub fn new(dialect: Arc<dyn Dialect>) -> Self {
        let syntax = dialect.syntax().clone();
        SqlLanguage { dialect, syntax, catalog: None, completion: CompletionOptions::default(), generation: 1, cache: RefCell::new(None) }
    }

    pub fn syntax(&self) -> &SqlSyntax {
        &self.syntax
    }

    /// Switch to another connection's dialect.
    pub fn set_dialect(&mut self, dialect: Arc<dyn Dialect>) {
        self.syntax = dialect.syntax().clone();
        self.dialect = dialect;
        self.generation += 1;
        self.cache.borrow_mut().take();
    }

    pub fn set_catalog(&mut self, catalog: Option<Arc<SnapshotCatalog>>) {
        self.catalog = catalog;
    }

    pub fn catalog(&self) -> Option<&Arc<SnapshotCatalog>> {
        self.catalog.as_ref()
    }
}

impl EditorLanguage for SqlLanguage {
    fn highlight(&self, text: &str) -> Vec<(Range<usize>, TokenStyle)> {
        let key = text_key(text);
        if let Some((k, v)) = self.cache.borrow().as_ref() {
            if *k == key {
                return v.clone();
            }
        }
        let v: Vec<(Range<usize>, TokenStyle)> = dbiji_sql::highlight(text, &self.syntax).into_iter().map(|(r, c)| (r, style(c))).collect();
        *self.cache.borrow_mut() = Some((key, v.clone()));
        v
    }

    fn complete(&self, text: &str, cursor: usize) -> Option<CompletionList> {
        let empty = SnapshotCatalog::new(Vec::new(), None);
        let catalog: &dyn CompletionCatalog = match &self.catalog {
            Some(c) => c.as_ref(),
            None => &empty,
        };
        let r = dbiji_sql::complete(text, cursor, self.dialect.as_ref(), catalog, &self.completion);
        if r.items.is_empty() {
            return None;
        }
        Some(CompletionList {
            replace: r.replace_start..r.replace_end,
            items: r
                .items
                .into_iter()
                .map(|i| {
                    let mut e = CompletionEntry::new(i.label, entry_kind(i.kind)).with_insert(i.insert).with_cursor_back(i.cursor_back);
                    if let Some(d) = i.detail {
                        e = e.with_detail(d);
                    }
                    e
                })
                .collect(),
        })
    }

    fn should_auto_complete(&self, text: &str, cursor: usize, typed: char) -> bool {
        if typed == '.' {
            return true;
        }
        // After "FROM " / "JOIN " / "INTO " / "UPDATE " offer tables right away.
        if typed == ' ' {
            let before = &text[..cursor.min(text.len())];
            let word = before.trim_end().rsplit(|c: char| c.is_whitespace() || c == '(').next().unwrap_or("");
            return matches!(word.to_ascii_uppercase().as_str(), "FROM" | "JOIN" | "INTO" | "UPDATE" | "TABLE");
        }
        false
    }

    fn hover(&self, text: &str, offset: usize) -> Option<String> {
        let obj = dbiji_sql::object_at(text, offset, &self.syntax)?;
        let catalog = self.catalog.as_ref()?;
        let table = catalog.find_table(obj.schema.as_deref(), &obj.table)?;
        let cols = catalog.columns(table.schema.as_deref(), &table.name).unwrap_or_default();
        match &obj.column {
            Some(c) => {
                let col = cols.iter().find(|x| x.name.eq_ignore_ascii_case(c))?;
                Some(format!("{}.{}  {}", table.name, col.name, col.data_type))
            }
            None => {
                let mut s = format!("{} {}", dbiji_core::tr(table.kind.label()), table.name);
                if let Some(c) = &table.comment {
                    s.push_str(&format!("\n{c}"));
                }
                for c in cols.iter().take(30) {
                    s.push_str(&format!("\n  {}  {}", c.name, c.data_type));
                }
                if cols.len() > 30 {
                    s.push_str(&format!("\n  … {}", cols.len() - 30));
                }
                Some(s)
            }
        }
    }

    fn link_at(&self, text: &str, offset: usize) -> Option<Range<usize>> {
        let obj = dbiji_sql::object_at(text, offset, &self.syntax)?;
        let catalog = self.catalog.as_ref()?;
        catalog.find_table(obj.schema.as_deref(), &obj.table)?;
        Some(obj.start..obj.end)
    }

    fn spell_suggestions(&self, word: &str) -> Vec<String> {
        dbiji_sql::SpellChecker::english().suggestions(word, 6)
    }

    fn highlight_generation(&self) -> u64 {
        self.generation
    }
}
