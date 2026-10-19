//! [`BasicSqlLanguage`]: a self-contained SQL highlighter with keyword and
//! (optional) table/column completion. DBiji adapts its full `dbiji-sql`
//! engine to [`EditorLanguage`]; this implementation is a dependency-free
//! fallback used by the demo, tests and benchmarks.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use crate::language::{CompletionEntry, CompletionList, EditorLanguage, EntryKind, TokenStyle};
use crate::text::{char_before, is_word_char, word_range_at, word_start_before};

/// A table known to [`BasicSqlLanguage`] completion.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TableInfo {
    /// Table name.
    pub name: String,
    /// Column names.
    pub columns: Vec<String>,
}

/// Small, fast non-cryptographic hasher (FxHash) for keyword lookups.
#[derive(Clone, Copy, Default)]
struct FxHasher(u64);

impl FxHasher {
    const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

    #[inline]
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(Self::SEED);
    }
}

impl std::hash::Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let (chunks, rest) = bytes.as_chunks::<8>();
        for c in chunks {
            self.add(u64::from_le_bytes(*c));
        }
        for &b in rest {
            self.add(u64::from(b));
        }
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(u64::from(i));
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

type FastSet = HashSet<String, std::hash::BuildHasherDefault<FxHasher>>;

/// Simple ANSI-ish SQL language support (see the module docs).
#[derive(Clone, Debug)]
pub struct BasicSqlLanguage {
    keywords: FastSet,
    functions: FastSet,
    types: FastSet,
    tables: Vec<TableInfo>,
    by_name: HashMap<String, usize>,
}

impl Default for BasicSqlLanguage {
    fn default() -> Self {
        Self::new()
    }
}

const MAX_WORD: usize = 64;

impl BasicSqlLanguage {
    /// Keywords, functions and types from `dbiji_core::keywords`.
    pub fn new() -> Self {
        use dbiji_core::keywords::{ANSI_FUNCTIONS, ANSI_KEYWORDS, ANSI_TYPES};
        let up = |s: &&str| s.to_ascii_uppercase();
        let mut types: FastSet = ANSI_TYPES.iter().map(up).collect();
        for t in [
            "VARCHAR",
            "BOOL",
            "JSON",
            "JSONB",
            "UUID",
            "SERIAL",
            "BIGSERIAL",
            "TIMESTAMPTZ",
            "BYTEA",
            "DATETIME",
            "DOUBLE",
        ] {
            types.insert(t.to_owned());
        }
        Self {
            keywords: ANSI_KEYWORDS.iter().map(up).collect(),
            functions: ANSI_FUNCTIONS.iter().map(up).collect(),
            types,
            tables: Vec::new(),
            by_name: HashMap::new(),
        }
    }

    /// Add tables for completion, hover and hyperlinks.
    pub fn with_tables(mut self, tables: Vec<TableInfo>) -> Self {
        for t in tables {
            self.by_name
                .insert(t.name.to_ascii_lowercase(), self.tables.len());
            self.tables.push(t);
        }
        self
    }

    fn table(&self, name: &str) -> Option<&TableInfo> {
        self.by_name
            .get(&name.trim_matches('"').to_ascii_lowercase())
            .map(|&i| &self.tables[i])
    }

    fn classify(&self, word: &str, next: Option<u8>) -> TokenStyle {
        if word.len() > MAX_WORD {
            return TokenStyle::Identifier;
        }
        let mut buf = [0u8; MAX_WORD];
        let bytes = word.as_bytes();
        for (i, b) in bytes.iter().enumerate() {
            buf[i] = b.to_ascii_uppercase();
        }
        let Ok(upper) = std::str::from_utf8(&buf[..bytes.len()]) else {
            return TokenStyle::Identifier;
        };
        if next == Some(b'(') && self.functions.contains(upper) {
            TokenStyle::Function
        } else if self.types.contains(upper) {
            TokenStyle::DataType
        } else if self.keywords.contains(upper) {
            TokenStyle::Keyword
        } else if next == Some(b'(') {
            TokenStyle::Function
        } else {
            TokenStyle::Identifier
        }
    }

    /// Aliases defined as `FROM table alias` / `JOIN table AS alias`.
    fn alias_target(&self, text: &str, alias: &str) -> Option<&TableInfo> {
        if let Some(t) = self.table(alias) {
            return Some(t);
        }
        let words: Vec<&str> = text
            .split(|c: char| !(is_word_char(c) || c == '"'))
            .filter(|w| !w.is_empty())
            .collect();
        let is = |i: usize, s: &str| words.get(i).is_some_and(|w| w.eq_ignore_ascii_case(s));
        for (i, t) in words.iter().enumerate() {
            let Some(table) = self.table(t) else { continue };
            if is(i + 1, alias) || (is(i + 1, "as") && is(i + 2, alias)) {
                return Some(table);
            }
        }
        None
    }
}

fn skip_quoted(b: &[u8], mut i: usize, quote: u8, backslash: bool) -> usize {
    // `i` points after the opening quote.
    while i < b.len() {
        let c = b[i];
        if backslash && c == b'\\' {
            i += 2;
            continue;
        }
        if c == quote {
            if b.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

impl EditorLanguage for BasicSqlLanguage {
    /// Multi-line statements (split at `;` outside strings and comments)
    /// and block comments.
    fn fold_ranges(&self, text: &str) -> Vec<Range<usize>> {
        let b = text.as_bytes();
        let mut out = Vec::new();
        let (mut i, mut stmt_start) = (0usize, None::<usize>);
        while i < b.len() {
            match b[i] {
                b'\'' => {
                    stmt_start.get_or_insert(i);
                    i += 1;
                    while i < b.len() && b[i] != b'\'' {
                        i += 1;
                    }
                }
                b'/' if b.get(i + 1) == Some(&b'*') => {
                    let start = i;
                    i += 2;
                    while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                        i += 1;
                    }
                    i = (i + 2).min(b.len());
                    if text[start..i].contains('\n') {
                        out.push(start..i);
                    }
                    continue;
                }
                b'-' if b.get(i + 1) == Some(&b'-') => {
                    while i < b.len() && b[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                b';' => {
                    if let Some(s) = stmt_start.take()
                        && text[s..i].contains('\n')
                    {
                        out.push(s..i + 1);
                    }
                }
                c if !c.is_ascii_whitespace() => {
                    stmt_start.get_or_insert(i);
                }
                _ => {}
            }
            i += 1;
        }
        if let Some(s) = stmt_start
            && text[s..].trim_end().contains('\n')
        {
            out.push(s..text.trim_end().len());
        }
        out
    }

    fn highlight(&self, text: &str) -> Vec<(Range<usize>, TokenStyle)> {
        let b = text.as_bytes();
        let mut out = Vec::with_capacity(text.len() / 4);
        let mut i = 0usize;
        let mut line_start = true;
        while i < b.len() {
            let c = b[i];
            let start = i;
            if c == b'\n' {
                line_start = true;
                i += 1;
                continue;
            }
            if c == b' ' || c == b'\t' || c == b'\r' {
                i += 1;
                continue;
            }
            let at_line_start = line_start;
            line_start = false;
            match c {
                b'-' if b.get(i + 1) == Some(&b'-') => {
                    let end = text[i..].find('\n').map_or(b.len(), |p| i + p);
                    out.push((start..end, TokenStyle::Comment));
                    i = end;
                }
                b'/' if b.get(i + 1) == Some(&b'*') => {
                    let end = text[i + 2..].find("*/").map_or(b.len(), |p| i + 2 + p + 2);
                    out.push((start..end, TokenStyle::Comment));
                    i = end;
                }
                b'\'' => {
                    i = skip_quoted(b, i + 1, b'\'', false);
                    out.push((start..i, TokenStyle::String));
                }
                b'"' => {
                    i = skip_quoted(b, i + 1, b'"', false);
                    out.push((start..i, TokenStyle::QuotedIdentifier));
                }
                b'`' => {
                    i = skip_quoted(b, i + 1, b'`', false);
                    out.push((start..i, TokenStyle::QuotedIdentifier));
                }
                b'$' => {
                    // $1 parameter, ${name} variable or $tag$ ... $tag$ string.
                    let rest = &text[i + 1..];
                    if rest.starts_with(|ch: char| ch.is_ascii_digit()) {
                        let n = rest
                            .find(|ch: char| !ch.is_ascii_digit())
                            .unwrap_or(rest.len());
                        i += 1 + n;
                        out.push((start..i, TokenStyle::Parameter));
                    } else if rest.starts_with('{') {
                        let end = rest.find('}').map_or(b.len(), |p| i + 1 + p + 1);
                        out.push((start..end, TokenStyle::Parameter));
                        i = end;
                    } else {
                        let tag_len = rest
                            .find(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
                            .unwrap_or(rest.len());
                        if rest[tag_len..].starts_with('$') {
                            let tag = &text[i..i + tag_len + 2];
                            let body = i + tag.len();
                            let end = text[body..]
                                .find(tag)
                                .map_or(b.len(), |p| body + p + tag.len());
                            out.push((start..end, TokenStyle::String));
                            i = end;
                        } else {
                            i += 1;
                            out.push((start..i, TokenStyle::Operator));
                        }
                    }
                }
                b'?' => {
                    i += 1;
                    out.push((start..i, TokenStyle::Parameter));
                }
                b':' if b
                    .get(i + 1)
                    .is_some_and(|n| n.is_ascii_alphabetic() || *n == b'_')
                    && (i == 0 || b[i - 1] != b':') =>
                {
                    let rest = &text[i + 1..];
                    let n = rest
                        .find(|ch: char| !is_word_char(ch))
                        .unwrap_or(rest.len());
                    i += 1 + n;
                    out.push((start..i, TokenStyle::Parameter));
                }
                b'@' => {
                    let rest = &text[i + 1..];
                    let at2 = usize::from(rest.starts_with('@'));
                    let n = rest[at2..]
                        .find(|ch: char| !is_word_char(ch))
                        .unwrap_or(rest.len() - at2);
                    let word = &rest[at2..at2 + n];
                    i += 1 + at2 + n;
                    let client = at_line_start
                        && at2 == 0
                        && matches!(
                            word.to_ascii_lowercase().as_str(),
                            "set" | "include" | "export" | "echo"
                        );
                    out.push((
                        start..i,
                        if client {
                            TokenStyle::ClientCommand
                        } else {
                            TokenStyle::Variable
                        },
                    ));
                }
                b'0'..=b'9' => {
                    if c == b'0' && matches!(b.get(i + 1), Some(b'x' | b'X')) {
                        i += 2;
                        while i < b.len() && b[i].is_ascii_hexdigit() {
                            i += 1;
                        }
                    } else {
                        while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                            i += 1;
                        }
                        if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
                            let mut j = i + 1;
                            if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
                                j += 1;
                            }
                            if j < b.len() && b[j].is_ascii_digit() {
                                i = j;
                                while i < b.len() && b[i].is_ascii_digit() {
                                    i += 1;
                                }
                            }
                        }
                    }
                    out.push((start..i, TokenStyle::Number));
                }
                b'(' | b')' | b',' | b';' | b'.' | b'[' | b']' | b'{' | b'}' => {
                    i += 1;
                    out.push((start..i, TokenStyle::Punctuation));
                }
                b'=' | b'<' | b'>' | b'!' | b'|' | b'&' | b'+' | b'-' | b'*' | b'/' | b'%'
                | b'^' | b'~' | b':' => {
                    i += 1;
                    while i < b.len() && matches!(b[i], b'=' | b'<' | b'>' | b'|' | b'&' | b':') {
                        i += 1;
                    }
                    out.push((start..i, TokenStyle::Operator));
                }
                _ => {
                    let ch = text[i..].chars().next().unwrap_or(' ');
                    if is_word_char(ch) {
                        let rest = &text[i..];
                        let n = rest
                            .find(|ch: char| !is_word_char(ch))
                            .unwrap_or(rest.len());
                        let word = &rest[..n];
                        // E'..' / N'..' / X'..' / B'..' prefixed strings.
                        if n == 1
                            && b.get(i + 1) == Some(&b'\'')
                            && matches!(c, b'E' | b'e' | b'N' | b'n' | b'X' | b'x' | b'B' | b'b')
                        {
                            i = skip_quoted(b, i + 2, b'\'', matches!(c, b'E' | b'e'));
                            out.push((start..i, TokenStyle::String));
                            continue;
                        }
                        let after = &text[i + n..];
                        let next = after.bytes().find(|b| *b != b' ');
                        let style = if at_line_start && word.eq_ignore_ascii_case("delimiter") {
                            TokenStyle::ClientCommand
                        } else {
                            self.classify(word, next)
                        };
                        i += n;
                        out.push((start..i, style));
                    } else {
                        i += ch.len_utf8();
                    }
                }
            }
        }
        out
    }

    fn complete(&self, text: &str, cursor: usize) -> Option<CompletionList> {
        let cursor = crate::text::floor_char_boundary(text, cursor);
        let start = word_start_before(text, cursor, is_word_char);
        let replace =
            start..word_range_at(text, cursor, is_word_char).map_or(cursor, |r| r.end.max(cursor));
        // Columns after `alias.` or `table.`
        if char_before(text, start) == Some('.') {
            let dot = start - 1;
            let q = word_start_before(text, dot, |c| is_word_char(c) || c == '"');
            let qualifier = &text[q..dot];
            let table = self.alias_target(text, qualifier)?;
            let items = table
                .columns
                .iter()
                .map(|c| {
                    CompletionEntry::new(c.clone(), EntryKind::Column)
                        .with_detail(table.name.clone())
                })
                .collect();
            return Some(CompletionList { replace, items });
        }
        let prev_word = text[..start]
            .split(|c: char| !is_word_char(c))
            .rev()
            .find(|w| !w.is_empty())
            .unwrap_or("");
        let wants_table = ["from", "join", "update", "into", "table"]
            .iter()
            .any(|k| prev_word.eq_ignore_ascii_case(k));
        let mut items: Vec<CompletionEntry> = Vec::new();
        let tables = self.tables.iter().map(|t| {
            CompletionEntry::new(t.name.clone(), EntryKind::Table)
                .with_detail(dbiji_core::trf("{} columns", &[&t.columns.len()]))
        });
        if wants_table {
            items.extend(tables);
        } else {
            let mut kws: Vec<&String> = self.keywords.iter().collect();
            kws.sort();
            items.extend(tables);
            for t in &self.tables {
                for c in &t.columns {
                    items.push(
                        CompletionEntry::new(c.clone(), EntryKind::Column)
                            .with_detail(t.name.clone()),
                    );
                }
            }
            let mut fns: Vec<&String> = self.functions.iter().collect();
            fns.sort();
            items.extend(fns.into_iter().map(|f| {
                let l = f.to_ascii_lowercase();
                CompletionEntry::new(l.clone(), EntryKind::Function)
                    .with_insert(format!("{l}()"))
                    .with_cursor_back(1)
            }));
            items.extend(
                kws.into_iter()
                    .map(|k| CompletionEntry::new(k.clone(), EntryKind::Keyword)),
            );
        }
        (!items.is_empty()).then_some(CompletionList { replace, items })
    }

    fn hover(&self, text: &str, offset: usize) -> Option<String> {
        let r = word_range_at(text, offset, is_word_char)?;
        let word = &text[r];
        if let Some(t) = self.table(word) {
            return Some(format!(
                "{}\n{}",
                dbiji_core::trf("Table {}", &[&t.name]),
                t.columns.join(", ")
            ));
        }
        let upper = word.to_ascii_uppercase();
        if self.functions.contains(&upper) {
            return Some(dbiji_core::trf(
                "Function {}",
                &[&word.to_ascii_lowercase()],
            ));
        }
        None
    }

    fn link_at(&self, text: &str, offset: usize) -> Option<Range<usize>> {
        let r = word_range_at(text, offset, is_word_char)?;
        self.table(&text[r.clone()]).map(|_| r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styles(text: &str) -> Vec<(&str, TokenStyle)> {
        BasicSqlLanguage::new()
            .highlight(text)
            .into_iter()
            .map(|(r, s)| (&text[r], s))
            .collect()
    }

    #[test]
    fn tokens() {
        use TokenStyle::*;
        assert_eq!(
            styles("SELECT count(*), 'it''s', \"Col\" FROM t -- c"),
            vec![
                ("SELECT", Keyword),
                ("count", Function),
                ("(", Punctuation),
                ("*", Operator),
                (")", Punctuation),
                (",", Punctuation),
                ("'it''s'", String),
                (",", Punctuation),
                ("\"Col\"", QuotedIdentifier),
                ("FROM", Keyword),
                ("t", Identifier),
                ("-- c", Comment),
            ]
        );
        assert_eq!(
            styles("x::int"),
            vec![("x", Identifier), ("::", Operator), ("int", DataType)]
        );
        assert_eq!(
            styles("$1 ? :name ${v}"),
            vec![
                ("$1", Parameter),
                ("?", Parameter),
                (":name", Parameter),
                ("${v}", Parameter)
            ]
        );
        assert_eq!(
            styles("$fn$ body ' $fn$ x"),
            vec![("$fn$ body ' $fn$", String), ("x", Identifier)]
        );
        assert_eq!(
            styles("@set a = 1\n x = @v"),
            vec![
                ("@set", ClientCommand),
                ("a", Identifier),
                ("=", Operator),
                ("1", Number),
                ("x", Identifier),
                ("=", Operator),
                ("@v", Variable)
            ]
        );
        assert_eq!(
            styles("E'a\\'b' N'x' 1.5e-3 0xFF"),
            vec![
                ("E'a\\'b'", String),
                ("N'x'", String),
                ("1.5e-3", Number),
                ("0xFF", Number)
            ]
        );
        assert_eq!(
            styles("/* a\n b */ é"),
            vec![("/* a\n b */", Comment), ("é", Identifier)]
        );
        assert_eq!(styles("'unterminated"), vec![("'unterminated", String)]);
    }

    #[test]
    fn completion_tables_columns_and_aliases() {
        let lang = BasicSqlLanguage::new().with_tables(vec![
            TableInfo {
                name: "users".into(),
                columns: vec!["id".into(), "name".into()],
            },
            TableInfo {
                name: "orders".into(),
                columns: vec!["id".into(), "user_id".into()],
            },
        ]);
        let text = "SELECT u. FROM users u";
        let list = lang.complete(text, 9).expect("columns");
        assert_eq!(list.replace, 9..9);
        assert_eq!(
            list.items
                .iter()
                .map(|i| i.label.as_str())
                .collect::<Vec<_>>(),
            vec!["id", "name"]
        );
        let text = "SELECT * FROM us";
        let list = lang.complete(text, text.len()).expect("tables");
        assert_eq!(list.replace, 14..16);
        assert!(list.items.iter().all(|i| i.kind == EntryKind::Table));
        let list = lang.complete("sel", 3).expect("keywords");
        assert!(list.items.iter().any(|i| i.label == "SELECT"));
        let count = list
            .items
            .iter()
            .find(|i| i.label == "count")
            .expect("function");
        assert_eq!((count.insert.as_str(), count.cursor_back), ("count()", 1));
        assert_eq!(lang.link_at("select * from users", 16), Some(14..19));
        assert!(lang.hover("users", 1).is_some_and(|h| h.contains("name")));
        assert_eq!(lang.complete("x.", 2), None);
    }
}
