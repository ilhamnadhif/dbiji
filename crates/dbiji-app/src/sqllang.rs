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
    /// SQL templates offered by name (FR-S5).
    pub templates: Option<dbiji_config::TemplateLibrary>,
    pub kind: Option<dbiji_core::DbKind>,
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

/// Words of the shell language that start with the word before the cursor.
fn shell_completion(text: &str, cursor: usize, syntax: &dbiji_core::SqlSyntax) -> dbiji_sql::CompletionResult {
    use dbiji_sql::{CompletionItem, CompletionKind, CompletionResult};
    let cursor = cursor.min(text.len());
    let start = text[..cursor].rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$' || c == '.')).map(|i| i + 1).unwrap_or(0);
    let word = &text[start..cursor];
    let lower = word.to_ascii_lowercase();
    let mut items = Vec::new();
    for k in syntax.keywords.iter().chain(syntax.functions.iter()) {
        if lower.is_empty() || k.to_ascii_lowercase().starts_with(&lower) {
            // An exact prefix match scores higher, so the closest command is first.
            let score = if k.to_ascii_lowercase() == lower { 3 } else { 1 };
            items.push(CompletionItem { label: (*k).to_string(), insert: (*k).to_string(), kind: CompletionKind::Keyword, detail: None, score, cursor_back: 0 });
        }
    }
    items.sort_by(|a, b| b.score.cmp(&a.score).then(a.label.cmp(&b.label)));
    CompletionResult { replace_start: start, replace_end: cursor, items }
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
        SqlLanguage { dialect, syntax, catalog: None, completion: CompletionOptions::default(), templates: None, kind: None, generation: 1, cache: RefCell::new(None) }
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
        if let Some((k, v)) = self.cache.borrow().as_ref()
            && *k == key
        {
            return v.clone();
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
        // A shell language (MongoDB, Redis) has no SQL analysis: only its
        // keywords (methods, commands) and its templates are offered.
        let r = if self.syntax.shell.is_some() {
            shell_completion(text, cursor, &self.syntax)
        } else {
            dbiji_sql::complete(text, cursor, self.dialect.as_ref(), catalog, &self.completion)
        };
        // Templates whose name starts with the word before the cursor.
        let cursor = cursor.min(text.len());
        let word_start = text[..cursor].rfind(|c: char| !(c.is_alphanumeric() || c == '_')).map(|i| i + 1).unwrap_or(0);
        let word = &text[word_start..cursor];
        let mut items: Vec<CompletionEntry> = Vec::new();
        if let (Some(lib), false) = (&self.templates, word.is_empty()) {
            for t in lib.matching(word, self.kind) {
                let (body, at) = dbiji_config::expand(&t, &Default::default());
                let back = at.map(|a| body.chars().count().saturating_sub(a)).unwrap_or(0);
                items.push(CompletionEntry::new(t.name.clone(), EntryKind::Snippet).with_insert(body).with_detail(t.description.clone()).with_cursor_back(back));
            }
        }
        let replace = if items.is_empty() { r.replace_start..r.replace_end } else { word_start..cursor };
        if !items.is_empty() && (r.replace_start != word_start || r.replace_end != cursor) {
            // Different replace ranges: offer only the templates.
        } else {
            items.extend(r.items.into_iter().map(|i| {
                let mut e = CompletionEntry::new(i.label, entry_kind(i.kind)).with_insert(i.insert).with_cursor_back(i.cursor_back);
                if let Some(d) = i.detail {
                    e = e.with_detail(d);
                }
                e
            }));
        }
        if items.is_empty() {
            return None;
        }
        Some(CompletionList { replace, items })
    }

    fn should_auto_complete(&self, text: &str, cursor: usize, typed: char) -> bool {
        if typed == '.' {
            return true;
        }
        if self.syntax.shell.is_some() {
            // Shell methods follow `.`; commands start a line.
            return typed == ' ' && cursor <= text.len() && text[..cursor].trim_end_matches(' ').ends_with('\n');
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

    fn fold_ranges(&self, text: &str) -> Vec<Range<usize>> {
        let mut out: Vec<Range<usize>> = dbiji_sql::split_statements(text, &self.syntax, &dbiji_sql::SplitOptions::default())
            .into_iter()
            .filter(|s| text[s.start..s.end].contains('\n'))
            .map(|s| s.start..s.end)
            .collect();
        out.extend(
            dbiji_sql::tokenize(text, &self.syntax)
                .into_iter()
                .filter(|t| t.kind == dbiji_sql::TokenKind::BlockComment && text[t.start..t.end].contains('\n'))
                .map(|t| t.start..t.end),
        );
        out
    }

    fn spell_suggestions(&self, word: &str) -> Vec<String> {
        dbiji_sql::SpellChecker::english().suggestions(word, 6)
    }

    fn highlight_generation(&self) -> u64 {
        self.generation
    }
}

/// Highlighted, non-wrapping layout of `code` from its `tokens` (see [`EditorLanguage::highlight`]).
pub fn highlight_job(code: &str, tokens: &[(Range<usize>, TokenStyle)], theme: &dbiji_editor::EditorTheme, font: egui::FontId) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let fmt = |color| egui::TextFormat { font_id: font.clone(), color, ..Default::default() };
    let mut tokens = tokens.to_vec();
    tokens.sort_by_key(|(r, _)| r.start);
    let mut pos = 0;
    for (r, style) in tokens {
        if r.start < pos || r.end > code.len() || !code.is_char_boundary(r.start) || !code.is_char_boundary(r.end) {
            continue;
        }
        if r.start > pos {
            job.append(&code[pos..r.start], 0.0, fmt(theme.token_color(TokenStyle::Plain)));
        }
        job.append(&code[r.clone()], 0.0, fmt(theme.token_color(style)));
        pos = r.end;
    }
    if pos < code.len() {
        job.append(&code[pos..], 0.0, fmt(theme.token_color(TokenStyle::Plain)));
    }
    job.wrap.max_width = f32::INFINITY;
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_and_keywords_are_offered() {
        let mut lang = SqlLanguage::new(Arc::new(dbiji_core::dialect::GenericDialect::new()));
        lang.templates = Some(dbiji_config::TemplateLibrary::default());
        let list = lang.complete("sgb", 3).expect("proposals");
        assert_eq!(list.replace, 0..3);
        assert!(list.items.iter().any(|i| i.label == "sgb" && i.insert.contains("GROUP BY")), "{:?}", list.items.iter().map(|i| &i.label).collect::<Vec<_>>());
        let list = lang.complete("SELECT * FR", 11).expect("keywords");
        assert!(list.items.iter().any(|i| i.label.eq_ignore_ascii_case("FROM")));
        let hl = lang.highlight("select 1 -- note");
        assert!(hl.iter().any(|(_, s)| *s == TokenStyle::Keyword));
        assert!(hl.iter().any(|(_, s)| *s == TokenStyle::Comment));
    }

    #[test]
    fn a_shell_language_offers_its_own_words_and_snippets() {
        let mut lang = SqlLanguage::new(Arc::new(dbiji_driver_redis::RedisDialect::new()));
        lang.kind = Some(dbiji_core::DbKind::Redis);
        lang.templates = Some(dbiji_config::TemplateLibrary::default());
        // Redis commands and its own snippets, never SQL keywords or templates.
        let list = lang.complete("HG", 2).expect("proposals");
        assert!(list.items.iter().any(|i| i.label == "HGETALL"), "{:?}", list.items.iter().map(|i| &i.label).collect::<Vec<_>>());
        assert!(list.items.iter().any(|i| i.label == "hgetall" && i.insert.starts_with("HGETALL ")), "{:?}", list.items.iter().map(|i| (&i.label, &i.insert)).collect::<Vec<_>>());
        assert!(!list.items.iter().any(|i| i.label.eq_ignore_ascii_case("SELECT")));
        assert!(!list.items.iter().any(|i| i.label == "sel"), "{:?}", list.items.iter().map(|i| &i.label).collect::<Vec<_>>());
        // Highlighting follows the same syntax.
        assert!(lang.highlight("GET user:1").iter().any(|(_, s)| *s == TokenStyle::Keyword));
    }
}
