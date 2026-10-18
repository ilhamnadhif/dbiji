//! Parsers for the `CREATE TABLE / INDEX / TRIGGER / VIEW` statements kept in
//! `sqlite_schema.sql`.
//!
//! SQLite's pragmas do not expose everything a [`dbiji_core::ddl::TableDef`]
//! needs (constraint names, CHECK expressions, generated column expressions,
//! AUTOINCREMENT, `DEFERRABLE INITIALLY DEFERRED`, ON CONFLICT clauses,
//! partial index predicates, expression index terms), so these are extracted
//! from the stored SQL with a token based parser. The parsers are tolerant:
//! unknown tokens are skipped and malformed input yields `None` instead of an
//! error, and callers fall back to pragma information.

use crate::lexer::{TokKind, Token, tokenize};

/// Token cursor over the significant tokens of a statement; keeps the full
/// token list to recover comments and the original text of sub-expressions.
pub(crate) struct Parser<'a> {
    src: &'a str,
    all: Vec<Token<'a>>,
    /// Indices into `all` of the significant tokens.
    sig: Vec<usize>,
    pos: usize,
}

impl<'a> Parser<'a> {
    pub fn new(src: &'a str) -> Self {
        let all = tokenize(src);
        let sig = all
            .iter()
            .enumerate()
            .filter(|(_, t)| !t.is_trivia())
            .map(|(i, _)| i)
            .collect();
        Parser {
            src,
            all,
            sig,
            pos: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.sig.len()
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn set_pos(&mut self, pos: usize) {
        self.pos = pos.min(self.sig.len());
    }

    /// Significant token number `i`.
    pub fn tok(&self, i: usize) -> Option<&Token<'a>> {
        self.sig.get(i).and_then(|&j| self.all.get(j))
    }

    pub fn peek(&self) -> Option<&Token<'a>> {
        self.tok(self.pos)
    }

    pub fn peek_at(&self, n: usize) -> Option<&Token<'a>> {
        self.tok(self.pos + n)
    }

    pub fn is_eof(&self) -> bool {
        self.pos >= self.sig.len()
    }

    pub fn bump(&mut self) -> Option<Token<'a>> {
        let t = self.peek().copied();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    pub fn at_word(&self, kw: &str) -> bool {
        self.peek().is_some_and(|t| t.is_word(kw))
    }

    pub fn at_kind(&self, kind: TokKind) -> bool {
        self.peek().is_some_and(|t| t.kind == kind)
    }

    pub fn eat_word(&mut self, kw: &str) -> bool {
        if self.at_word(kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Consume the whole keyword sequence or nothing.
    pub fn eat_words(&mut self, kws: &[&str]) -> bool {
        let ok = kws
            .iter()
            .enumerate()
            .all(|(i, kw)| self.peek_at(i).is_some_and(|t| t.is_word(kw)));
        if ok {
            self.pos += kws.len();
        }
        ok
    }

    /// Name token (word, quoted identifier or string).
    pub fn name(&mut self) -> Option<String> {
        let v = self.peek()?.ident()?;
        self.pos += 1;
        Some(v)
    }

    /// `[schema .] name`; returns the schema, the name and the range of
    /// significant token indices covering the name.
    pub fn qualified_name(&mut self) -> Option<(Option<String>, String, (usize, usize))> {
        let start = self.pos;
        let first = self.name()?;
        if self.at_kind(TokKind::Dot) {
            self.pos += 1;
            let second = self.name()?;
            Some((Some(first), second, (start, self.pos)))
        } else {
            Some((None, first, (start, self.pos)))
        }
    }

    /// At `(`: skip the parenthesised group and return the range of token
    /// indices strictly inside it. `None` when not at `(` or unbalanced.
    pub fn group(&mut self) -> Option<(usize, usize)> {
        if !self.at_kind(TokKind::LParen) {
            return None;
        }
        let open = self.pos;
        let mut depth = 0i32;
        let mut i = self.pos;
        while let Some(t) = self.tok(i) {
            match t.kind {
                TokKind::LParen => depth += 1,
                TokKind::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        self.pos = i + 1;
                        return Some((open + 1, i));
                    }
                }
                _ => {}
            }
            i += 1;
        }
        None
    }

    /// Original source text spanning significant tokens `[from, to)`,
    /// including the trivia between them.
    pub fn text(&self, from: usize, to: usize) -> &'a str {
        if from >= to {
            return "";
        }
        match (self.tok(from), self.tok(to - 1)) {
            (Some(a), Some(b)) => &self.src[a.start..b.end()],
            _ => "",
        }
    }

    /// Source text from token `from` to the end of the input.
    pub fn text_from(&self, from: usize) -> &'a str {
        match self.tok(from) {
            Some(t) => self.src[t.start..].trim_end(),
            None => "",
        }
    }

    /// Split the significant token range `[from, to)` on commas at nesting level 0.
    pub fn split_commas(&self, from: usize, to: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut depth = 0i32;
        let mut start = from;
        for i in from..to {
            match self.tok(i).map(|t| t.kind) {
                Some(TokKind::LParen) => depth += 1,
                Some(TokKind::RParen) => depth -= 1,
                Some(TokKind::Comma) if depth == 0 => {
                    out.push((start, i));
                    start = i + 1;
                }
                _ => {}
            }
        }
        if start < to {
            out.push((start, to));
        }
        out
    }

    /// Comment written after significant token `last` on the same line,
    /// optionally after a comma: `a INT, -- the comment`.
    pub fn trailing_comment(&self, last: usize) -> Option<String> {
        let mut j = *self.sig.get(last)? + 1;
        let mut seen_comma = false;
        while let Some(t) = self.all.get(j) {
            match t.kind {
                TokKind::Space if t.text.contains('\n') => return None,
                TokKind::Space => {}
                TokKind::Comma if !seen_comma => seen_comma = true,
                TokKind::Comment => return comment_text(t.text),
                _ => return None,
            }
            j += 1;
        }
        None
    }
}

/// Text of a comment without its markers; `None` when empty.
fn comment_text(c: &str) -> Option<String> {
    let inner = if let Some(rest) = c.strip_prefix("--") {
        rest
    } else if let Some(rest) = c.strip_prefix("/*") {
        rest.strip_suffix("*/").unwrap_or(rest)
    } else {
        c
    };
    let t = inner.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Normalise whitespace runs to single spaces.
pub(crate) fn squash_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `ON CONFLICT <algorithm>` of a constraint.
fn conflict_clause(p: &mut Parser<'_>) -> Option<String> {
    if p.eat_words(&["ON", "CONFLICT"]) {
        p.bump().map(|t| t.text.to_ascii_uppercase())
    } else {
        None
    }
}

/// One term of an index / PRIMARY KEY / UNIQUE column list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct IndexedTerm {
    /// Column name, or expression text when `is_expression`.
    pub name: String,
    pub is_expression: bool,
    pub collation: Option<String>,
    pub descending: bool,
    /// `AUTOINCREMENT` inside a table level `PRIMARY KEY (id AUTOINCREMENT)`.
    pub autoincrement: bool,
}

fn indexed_terms(p: &Parser<'_>, from: usize, to: usize) -> Vec<IndexedTerm> {
    let mut out = Vec::new();
    for (s, mut e) in p.split_commas(from, to) {
        let mut term = IndexedTerm::default();
        if e > s && p.tok(e - 1).is_some_and(|t| t.is_word("AUTOINCREMENT")) {
            term.autoincrement = true;
            e -= 1;
        }
        if e > s
            && p.tok(e - 1)
                .is_some_and(|t| t.is_any_word(&["ASC", "DESC"]))
        {
            term.descending = p.tok(e - 1).is_some_and(|t| t.is_word("DESC"));
            e -= 1;
        }
        if e >= s + 2 && p.tok(e - 2).is_some_and(|t| t.is_word("COLLATE")) {
            term.collation = p.tok(e - 1).and_then(Token::ident);
            e -= 2;
        }
        if e == s + 1 && p.tok(s).is_some_and(|t| t.ident().is_some()) {
            term.name = p.tok(s).and_then(Token::ident).unwrap_or_default();
        } else {
            term.name = p.text(s, e).to_string();
            term.is_expression = true;
        }
        if !term.name.is_empty() {
            out.push(term);
        }
    }
    out
}

fn name_list(p: &Parser<'_>, from: usize, to: usize) -> Vec<String> {
    p.split_commas(from, to)
        .into_iter()
        .filter_map(|(s, e)| {
            if e == s + 1 {
                p.tok(s).and_then(Token::ident)
            } else {
                None
            }
        })
        .collect()
}

/// `REFERENCES` clause of a foreign key.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ParsedFk {
    pub name: Option<String>,
    /// Child columns (table level constraints; the column itself for column constraints).
    pub columns: Vec<String>,
    pub ref_table: String,
    /// Empty when the parent's primary key is referenced implicitly.
    pub ref_columns: Vec<String>,
    pub on_delete: Option<String>,
    pub on_update: Option<String>,
    /// `DEFERRABLE INITIALLY DEFERRED` (the only deferred form in SQLite).
    pub deferred: bool,
}

fn fk_action(p: &mut Parser<'_>) -> Option<String> {
    if p.eat_words(&["SET", "NULL"]) {
        Some("SET NULL".into())
    } else if p.eat_words(&["SET", "DEFAULT"]) {
        Some("SET DEFAULT".into())
    } else if p.eat_words(&["NO", "ACTION"]) {
        Some("NO ACTION".into())
    } else if p.eat_word("CASCADE") {
        Some("CASCADE".into())
    } else if p.eat_word("RESTRICT") {
        Some("RESTRICT".into())
    } else {
        None
    }
}

/// Parse `REFERENCES table [(cols)] [ON ... | MATCH x | [NOT] DEFERRABLE ...]*`.
fn references_clause(p: &mut Parser<'_>) -> Option<ParsedFk> {
    if !p.eat_word("REFERENCES") {
        return None;
    }
    let mut fk = ParsedFk {
        ref_table: p.name()?,
        ..Default::default()
    };
    if let Some((s, e)) = p.group() {
        fk.ref_columns = name_list(p, s, e);
    }
    loop {
        if p.eat_word("ON") {
            if p.eat_word("DELETE") {
                fk.on_delete = fk_action(p);
            } else if p.eat_word("UPDATE") {
                fk.on_update = fk_action(p);
            } else {
                p.bump();
            }
        } else if p.eat_word("MATCH") {
            p.bump();
        } else if p.at_word("NOT") && p.peek_at(1).is_some_and(|t| t.is_word("DEFERRABLE")) {
            p.bump();
            p.bump();
            if p.eat_word("INITIALLY") {
                p.bump();
            }
            fk.deferred = false;
        } else if p.eat_word("DEFERRABLE") {
            fk.deferred = p.eat_words(&["INITIALLY", "DEFERRED"]);
            if !fk.deferred && p.eat_word("INITIALLY") {
                p.bump();
            }
        } else {
            break;
        }
    }
    Some(fk)
}

/// Named or unnamed constraint with an optional conflict clause.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct NamedConstraint {
    pub name: Option<String>,
    pub conflict: Option<String>,
}

/// CHECK constraint.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ParsedCheck {
    pub name: Option<String>,
    pub expression: String,
}

/// Column level PRIMARY KEY.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ColumnPk {
    pub name: Option<String>,
    pub conflict: Option<String>,
    pub descending: bool,
    pub autoincrement: bool,
}

/// One column definition of `CREATE TABLE`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ParsedColumn {
    pub name: String,
    /// Declared type as written (whitespace normalised); empty when omitted.
    pub type_name: String,
    pub primary_key: Option<ColumnPk>,
    pub not_null: Option<NamedConstraint>,
    pub unique: Vec<NamedConstraint>,
    pub checks: Vec<ParsedCheck>,
    /// Default expression; outer parentheses removed (as `PRAGMA table_info` reports it).
    pub default: Option<String>,
    pub collation: Option<String>,
    pub references: Vec<ParsedFk>,
    /// Generated column expression and whether it is `STORED`.
    pub generated: Option<(String, bool)>,
    pub comment: Option<String>,
}

/// Table level constraint of `CREATE TABLE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ParsedTableConstraint {
    PrimaryKey {
        name: Option<String>,
        columns: Vec<IndexedTerm>,
        conflict: Option<String>,
    },
    Unique {
        name: Option<String>,
        columns: Vec<IndexedTerm>,
        conflict: Option<String>,
    },
    Check(ParsedCheck),
    ForeignKey(ParsedFk),
}

/// Parsed `CREATE [VIRTUAL] TABLE`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ParsedTable {
    pub name: String,
    pub temporary: bool,
    pub columns: Vec<ParsedColumn>,
    pub constraints: Vec<ParsedTableConstraint>,
    pub without_rowid: bool,
    pub strict: bool,
    /// `USING module(args)` of a virtual table.
    pub virtual_module: Option<(String, String)>,
}

impl ParsedTable {
    pub fn column(&self, name: &str) -> Option<&ParsedColumn> {
        self.columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// All CHECK constraints, column level first, in declaration order.
    pub fn checks(&self) -> Vec<ParsedCheck> {
        let mut out: Vec<ParsedCheck> = self
            .columns
            .iter()
            .flat_map(|c| c.checks.iter().cloned())
            .collect();
        for c in &self.constraints {
            if let ParsedTableConstraint::Check(ch) = c {
                out.push(ch.clone());
            }
        }
        out
    }

    /// All foreign keys in declaration order (column level constraints get
    /// their column filled in).
    pub fn foreign_keys(&self) -> Vec<ParsedFk> {
        let mut out = Vec::new();
        for c in &self.columns {
            for r in &c.references {
                let mut fk = r.clone();
                fk.columns = vec![c.name.clone()];
                out.push(fk);
            }
        }
        for c in &self.constraints {
            if let ParsedTableConstraint::ForeignKey(fk) = c {
                out.push(fk.clone());
            }
        }
        out
    }

    /// Name and conflict clause of the primary key.
    pub fn primary_key_info(&self) -> Option<NamedConstraint> {
        for c in &self.columns {
            if let Some(pk) = &c.primary_key {
                return Some(NamedConstraint {
                    name: pk.name.clone(),
                    conflict: pk.conflict.clone(),
                });
            }
        }
        self.constraints.iter().find_map(|c| match c {
            ParsedTableConstraint::PrimaryKey { name, conflict, .. } => Some(NamedConstraint {
                name: name.clone(),
                conflict: conflict.clone(),
            }),
            _ => None,
        })
    }

    /// Columns of the primary key in key order.
    pub fn primary_key_columns(&self) -> Vec<String> {
        for c in &self.columns {
            if c.primary_key.is_some() {
                return vec![c.name.clone()];
            }
        }
        self.constraints
            .iter()
            .find_map(|c| match c {
                ParsedTableConstraint::PrimaryKey { columns, .. } => Some(
                    columns
                        .iter()
                        .filter(|t| !t.is_expression)
                        .map(|t| t.name.clone())
                        .collect(),
                ),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Column using AUTOINCREMENT, if any.
    pub fn autoincrement_column(&self) -> Option<String> {
        for c in &self.columns {
            if c.primary_key.as_ref().is_some_and(|pk| pk.autoincrement) {
                return Some(c.name.clone());
            }
        }
        self.constraints.iter().find_map(|c| match c {
            ParsedTableConstraint::PrimaryKey { columns, .. }
                if columns.len() == 1 && columns[0].autoincrement =>
            {
                Some(columns[0].name.clone())
            }
            _ => None,
        })
    }

    /// UNIQUE constraints as (name, columns, conflict), column level first.
    pub fn unique_constraints(&self) -> Vec<(Option<String>, Vec<String>, Option<String>)> {
        let mut out = Vec::new();
        for c in &self.columns {
            for u in &c.unique {
                out.push((u.name.clone(), vec![c.name.clone()], u.conflict.clone()));
            }
        }
        for c in &self.constraints {
            if let ParsedTableConstraint::Unique {
                name,
                columns,
                conflict,
            } = c
            {
                out.push((
                    name.clone(),
                    columns.iter().map(|t| t.name.clone()).collect(),
                    conflict.clone(),
                ));
            }
        }
        out
    }
}

const COLUMN_CONSTRAINT_START: &[&str] = &[
    "CONSTRAINT",
    "PRIMARY",
    "NOT",
    "NULL",
    "UNIQUE",
    "CHECK",
    "DEFAULT",
    "COLLATE",
    "REFERENCES",
    "GENERATED",
    "AS",
];

const TABLE_CONSTRAINT_START: &[&str] = &["CONSTRAINT", "PRIMARY", "UNIQUE", "CHECK", "FOREIGN"];

/// Skip `CREATE [TEMP|TEMPORARY] [UNIQUE] [VIRTUAL] <object> [IF NOT EXISTS]`.
/// Returns (temporary, unique, virtual) or `None` when `object` does not match.
fn create_prefix(p: &mut Parser<'_>, object: &str) -> Option<(bool, bool, bool)> {
    if !p.eat_word("CREATE") {
        return None;
    }
    let temporary = p.eat_word("TEMP") || p.eat_word("TEMPORARY");
    let unique = p.eat_word("UNIQUE");
    let virtual_ = p.eat_word("VIRTUAL");
    if !p.eat_word(object) {
        return None;
    }
    p.eat_words(&["IF", "NOT", "EXISTS"]);
    Some((temporary, unique, virtual_))
}

fn parse_column(p: &mut Parser<'_>, s: usize, e: usize) -> Option<ParsedColumn> {
    p.set_pos(s);
    let mut col = ParsedColumn {
        name: p.name()?,
        ..Default::default()
    };
    // type name: words until a constraint keyword, optionally followed by (n[, m])
    let type_start = p.pos();
    while p.pos() < e {
        let Some(t) = p.peek() else { break };
        let is_type_word = matches!(
            t.kind,
            TokKind::Word | TokKind::QuotedIdent | TokKind::String
        ) && !t.is_any_word(COLUMN_CONSTRAINT_START);
        if !is_type_word {
            break;
        }
        p.bump();
    }
    if p.pos() > type_start && p.pos() < e && p.at_kind(TokKind::LParen) {
        p.group();
    }
    col.type_name = squash_ws(p.text(type_start, p.pos().min(e)));
    // constraints
    while p.pos() < e {
        let name = if p.eat_word("CONSTRAINT") {
            p.name()
        } else {
            None
        };
        if p.pos() >= e {
            break;
        }
        if p.eat_words(&["PRIMARY", "KEY"]) {
            let mut pk = ColumnPk {
                name,
                ..Default::default()
            };
            if p.eat_word("DESC") {
                pk.descending = true;
            } else {
                p.eat_word("ASC");
            }
            pk.conflict = conflict_clause(p);
            pk.autoincrement = p.eat_word("AUTOINCREMENT");
            col.primary_key = Some(pk);
        } else if p.eat_words(&["NOT", "NULL"]) {
            col.not_null = Some(NamedConstraint {
                name,
                conflict: conflict_clause(p),
            });
        } else if p.eat_word("NULL") {
            conflict_clause(p);
        } else if p.eat_word("UNIQUE") {
            col.unique.push(NamedConstraint {
                name,
                conflict: conflict_clause(p),
            });
        } else if p.eat_word("CHECK") {
            if let Some((gs, ge)) = p.group() {
                col.checks.push(ParsedCheck {
                    name,
                    expression: p.text(gs, ge).trim().to_string(),
                });
            }
        } else if p.eat_word("DEFAULT") {
            if p.at_kind(TokKind::LParen) {
                if let Some((gs, ge)) = p.group() {
                    col.default = Some(p.text(gs, ge).trim().to_string());
                }
            } else {
                let ds = p.pos();
                if p.peek().is_some_and(|t| {
                    t.kind == TokKind::Operator && (t.text == "-" || t.text == "+")
                }) {
                    p.bump();
                }
                p.bump();
                col.default = Some(p.text(ds, p.pos()).to_string());
            }
        } else if p.eat_word("COLLATE") {
            col.collation = p.name();
        } else if p.at_word("REFERENCES") {
            if let Some(mut fk) = references_clause(p) {
                fk.name = name;
                col.references.push(fk);
            }
        } else if p.at_word("GENERATED") || p.at_word("AS") {
            p.eat_words(&["GENERATED", "ALWAYS"]);
            if !p.eat_word("AS") {
                p.bump();
                continue;
            }
            if let Some((gs, ge)) = p.group() {
                let expr = p.text(gs, ge).trim().to_string();
                let stored = p.eat_word("STORED");
                if !stored {
                    p.eat_word("VIRTUAL");
                }
                col.generated = Some((expr, stored));
            }
        } else {
            // unknown token: skip it
            p.bump();
        }
    }
    col.comment = p.trailing_comment(e.saturating_sub(1));
    Some(col)
}

fn parse_table_constraint(p: &mut Parser<'_>, s: usize, e: usize) -> Option<ParsedTableConstraint> {
    p.set_pos(s);
    let name = if p.eat_word("CONSTRAINT") {
        p.name()
    } else {
        None
    };
    if p.eat_words(&["PRIMARY", "KEY"]) {
        let (gs, ge) = p.group()?;
        let columns = indexed_terms(p, gs, ge);
        let conflict = conflict_clause(p);
        Some(ParsedTableConstraint::PrimaryKey {
            name,
            columns,
            conflict,
        })
    } else if p.eat_word("UNIQUE") {
        let (gs, ge) = p.group()?;
        let columns = indexed_terms(p, gs, ge);
        let conflict = conflict_clause(p);
        Some(ParsedTableConstraint::Unique {
            name,
            columns,
            conflict,
        })
    } else if p.eat_word("CHECK") {
        let (gs, ge) = p.group()?;
        Some(ParsedTableConstraint::Check(ParsedCheck {
            name,
            expression: p.text(gs, ge).trim().to_string(),
        }))
    } else if p.eat_words(&["FOREIGN", "KEY"]) {
        let (gs, ge) = p.group()?;
        let columns = name_list(p, gs, ge);
        let mut fk = references_clause(p)?;
        fk.name = name;
        fk.columns = columns;
        let _ = e;
        Some(ParsedTableConstraint::ForeignKey(fk))
    } else {
        None
    }
}

/// Parse a `CREATE TABLE` or `CREATE VIRTUAL TABLE` statement.
pub(crate) fn parse_create_table(sql: &str) -> Option<ParsedTable> {
    let mut p = Parser::new(sql);
    let (temporary, _, virtual_) = create_prefix(&mut p, "TABLE")?;
    let (_, name, _) = p.qualified_name()?;
    let mut table = ParsedTable {
        name,
        temporary,
        ..Default::default()
    };
    if virtual_ {
        if !p.eat_word("USING") {
            return None;
        }
        let module = p.name()?;
        let args = match p.group() {
            Some((s, e)) => p.text(s, e).trim().to_string(),
            None => String::new(),
        };
        table.virtual_module = Some((module, args));
        return Some(table);
    }
    let (gs, ge) = p.group()?;
    for (s, e) in p.split_commas(gs, ge) {
        let is_constraint = p
            .tok(s)
            .is_some_and(|t| t.is_any_word(TABLE_CONSTRAINT_START));
        if is_constraint {
            if let Some(c) = parse_table_constraint(&mut p, s, e) {
                table.constraints.push(c);
            }
        } else if let Some(c) = parse_column(&mut p, s, e) {
            table.columns.push(c);
        }
    }
    p.set_pos(ge + 1);
    while !p.is_eof() {
        if p.eat_words(&["WITHOUT", "ROWID"]) {
            table.without_rowid = true;
        } else if p.eat_word("STRICT") {
            table.strict = true;
        } else {
            p.bump();
        }
    }
    Some(table)
}

/// Parsed `CREATE INDEX`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ParsedIndex {
    pub unique: bool,
    pub name: String,
    pub table: String,
    pub columns: Vec<IndexedTerm>,
    pub predicate: Option<String>,
    /// Significant token range of the (possibly qualified) index name.
    pub name_range: (usize, usize),
    /// Significant token index of the table name after `ON`.
    pub table_token: usize,
}

/// Parse a `CREATE [UNIQUE] INDEX` statement.
pub(crate) fn parse_create_index(sql: &str) -> Option<ParsedIndex> {
    let mut p = Parser::new(sql);
    let (_, unique, _) = create_prefix(&mut p, "INDEX")?;
    let (_, name, name_range) = p.qualified_name()?;
    if !p.eat_word("ON") {
        return None;
    }
    let table_token = p.pos();
    let table = p.name()?;
    let (gs, ge) = p.group()?;
    let columns = indexed_terms(&p, gs, ge);
    let predicate = if p.eat_word("WHERE") {
        let s = p.pos();
        let text = p.text_from(s);
        let text = text.strip_suffix(';').unwrap_or(text).trim();
        if text.is_empty() {
            None
        } else {
            Some(text.to_string())
        }
    } else {
        None
    };
    Some(ParsedIndex {
        unique,
        name,
        table,
        columns,
        predicate,
        name_range,
        table_token,
    })
}

/// Parsed `CREATE TRIGGER`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ParsedTrigger {
    pub name: String,
    pub temporary: bool,
    /// BEFORE / AFTER / INSTEAD OF
    pub timing: String,
    /// INSERT / DELETE / UPDATE / `UPDATE OF a, b`
    pub event: String,
    pub table: String,
    pub for_each_row: bool,
    pub when: Option<String>,
    /// Statements between BEGIN and END.
    pub body: String,
    /// Significant token range of the (possibly qualified) trigger name.
    pub name_range: (usize, usize),
    /// Significant token index of the table name after `ON`.
    pub table_token: usize,
}

/// Parse a `CREATE TRIGGER` statement.
pub(crate) fn parse_create_trigger(sql: &str) -> Option<ParsedTrigger> {
    let mut p = Parser::new(sql);
    let (temporary, _, _) = create_prefix(&mut p, "TRIGGER")?;
    let (_, name, name_range) = p.qualified_name()?;
    let timing = if p.eat_word("BEFORE") {
        "BEFORE"
    } else if p.eat_word("AFTER") {
        "AFTER"
    } else if p.eat_words(&["INSTEAD", "OF"]) {
        "INSTEAD OF"
    } else {
        "BEFORE"
    };
    let event = if p.eat_word("INSERT") {
        "INSERT".to_string()
    } else if p.eat_word("DELETE") {
        "DELETE".to_string()
    } else if p.eat_word("UPDATE") {
        if p.eat_word("OF") {
            let s = p.pos();
            while !p.is_eof() && !p.at_word("ON") {
                p.bump();
            }
            format!("UPDATE OF {}", squash_ws(p.text(s, p.pos())))
        } else {
            "UPDATE".to_string()
        }
    } else {
        return None;
    };
    if !p.eat_word("ON") {
        return None;
    }
    let table_token = p.pos();
    let (_, table, _) = p.qualified_name()?;
    let for_each_row = p.eat_words(&["FOR", "EACH", "ROW"]);
    let when = if p.eat_word("WHEN") {
        let s = p.pos();
        while !p.is_eof() && !p.at_word("BEGIN") {
            p.bump();
        }
        Some(p.text(s, p.pos()).trim().to_string())
    } else {
        None
    };
    if !p.eat_word("BEGIN") {
        return None;
    }
    let body_start = p.pos();
    // body ends at the last END token
    let mut end = p.len();
    while end > body_start {
        if p.tok(end - 1).is_some_and(|t| t.is_word("END")) {
            break;
        }
        end -= 1;
    }
    let body = p
        .text(body_start, end.saturating_sub(1).max(body_start))
        .trim()
        .to_string();
    Some(ParsedTrigger {
        name,
        temporary,
        timing: timing.to_string(),
        event,
        table,
        for_each_row,
        when,
        body,
        name_range,
        table_token,
    })
}

/// Parsed `CREATE VIEW`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ParsedView {
    pub name: String,
    pub temporary: bool,
    /// Explicit column list `(a, b)`.
    pub columns: Option<Vec<String>>,
    /// The SELECT statement.
    pub select: String,
}

/// Parse a `CREATE VIEW` statement.
pub(crate) fn parse_create_view(sql: &str) -> Option<ParsedView> {
    let mut p = Parser::new(sql);
    let (temporary, _, _) = create_prefix(&mut p, "VIEW")?;
    let (_, name, _) = p.qualified_name()?;
    let columns = p.group().map(|(s, e)| name_list(&p, s, e));
    if !p.eat_word("AS") {
        return None;
    }
    let text = p.text_from(p.pos());
    let select = text.strip_suffix(';').unwrap_or(text).trim().to_string();
    Some(ParsedView {
        name,
        temporary,
        columns,
        select,
    })
}

/// Rewrite the object name (and for indexes / triggers the table after `ON`)
/// of a stored `CREATE INDEX` / `CREATE TRIGGER` / `CREATE VIEW` statement.
/// `qualified_name` replaces the `[schema.]name` tokens, `table` the table
/// name token. `TEMP` / `TEMPORARY` is removed when `drop_temp` is set
/// (the qualified name then decides the schema).
pub(crate) fn rewrite_create(
    sql: &str,
    object: &str,
    qualified_name: &str,
    table: Option<&str>,
    drop_temp: bool,
) -> Option<String> {
    let mut p = Parser::new(sql);
    if !p.at_word("CREATE") {
        return None;
    }
    let create_tok = p.tok(0).copied()?;
    p.bump();
    let mut temp_tok = None;
    if p.at_word("TEMP") || p.at_word("TEMPORARY") {
        temp_tok = p.bump();
    }
    p.eat_word("UNIQUE");
    if !p.eat_word(object) {
        return None;
    }
    p.eat_words(&["IF", "NOT", "EXISTS"]);
    let (_, _, (ns, ne)) = p.qualified_name()?;
    let name_start = p.tok(ns)?.start;
    let name_end = p.tok(ne - 1)?.end();
    let mut table_span = None;
    if let Some(new_table) = table {
        // find ON <table> after the name (indexes: right after; triggers: after the event)
        let mut i = ne;
        while let Some(t) = p.tok(i) {
            if t.is_word("ON") {
                let mut q = Parser::new(sql);
                q.set_pos(i + 1);
                let (_, _, (ts, te)) = q.qualified_name()?;
                table_span = Some((p.tok(ts)?.start, p.tok(te - 1)?.end(), new_table));
                break;
            }
            if t.is_word("BEGIN")
                || t.kind == TokKind::LParen && object.eq_ignore_ascii_case("VIEW")
            {
                break;
            }
            i += 1;
        }
    }
    let mut out = String::with_capacity(sql.len() + 32);
    let mut cursor = 0usize;
    if drop_temp && let Some(t) = temp_tok {
        out.push_str(&sql[cursor..create_tok.end()]);
        cursor = t.end();
    }
    out.push_str(&sql[cursor..name_start]);
    out.push_str(qualified_name);
    cursor = name_end;
    if let Some((ts, te, new_table)) = table_span {
        out.push_str(&sql[cursor..ts]);
        out.push_str(new_table);
        cursor = te;
    }
    out.push_str(&sql[cursor..]);
    Some(out)
}

/// Identifiers (bare or quoted, not followed by `(`) of an expression.
/// SQLite keywords that can never be used as an unquoted identifier (the
/// others fall back to identifiers in SQLite's grammar), plus TRUE / FALSE.
const RESERVED_WORDS: &[&str] = &[
    "ADD",
    "ALL",
    "ALTER",
    "AND",
    "AS",
    "AUTOINCREMENT",
    "BETWEEN",
    "CASE",
    "CHECK",
    "COLLATE",
    "COMMIT",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "CURRENT_DATE",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "DEFAULT",
    "DEFERRABLE",
    "DELETE",
    "DISTINCT",
    "DROP",
    "ELSE",
    "ESCAPE",
    "EXCEPT",
    "EXISTS",
    "FALSE",
    "FILTER",
    "FOREIGN",
    "FROM",
    "FULL",
    "GLOB",
    "GROUP",
    "HAVING",
    "IN",
    "INDEX",
    "INDEXED",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTO",
    "IS",
    "ISNULL",
    "JOIN",
    "LEFT",
    "LIKE",
    "LIMIT",
    "NATURAL",
    "NOT",
    "NOTHING",
    "NOTNULL",
    "NULL",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "OVER",
    "PRIMARY",
    "REFERENCES",
    "REGEXP",
    "RETURNING",
    "RIGHT",
    "ROLLBACK",
    "SELECT",
    "SET",
    "TABLE",
    "THEN",
    "TO",
    "TRANSACTION",
    "TRUE",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USING",
    "VALUES",
    "WHEN",
    "WHERE",
    "WINDOW",
];

/// Token that may name a column: a quoted identifier, or a bare word that is
/// not a reserved keyword.
fn is_identifier_token(t: &Token<'_>) -> bool {
    match t.kind {
        TokKind::QuotedIdent => true,
        TokKind::Word => !t.is_any_word(RESERVED_WORDS),
        _ => false,
    }
}

pub(crate) fn expression_identifiers(expr: &str) -> Vec<String> {
    let p = Parser::new(expr);
    let mut out = Vec::new();
    for i in 0..p.len() {
        let Some(t) = p.tok(i) else { continue };
        if !is_identifier_token(t) {
            continue;
        }
        if p.tok(i + 1).is_some_and(|n| n.kind == TokKind::LParen) {
            continue;
        }
        if let Some(v) = t.ident() {
            out.push(v);
        }
    }
    out
}

/// Replace column references in a single-table expression (CHECK, generated
/// column, index term or predicate): every identifier token equal to a key of
/// `renames` (case-insensitive) that is not a function name is replaced by the
/// quoted replacement.
pub(crate) fn rename_identifiers(
    expr: &str,
    renames: &[(String, String)],
    quote: &dyn Fn(&str) -> String,
) -> String {
    if renames.is_empty() {
        return expr.to_string();
    }
    let toks = tokenize(expr);
    let mut out = String::with_capacity(expr.len() + 8);
    for (i, t) in toks.iter().enumerate() {
        let is_name = is_identifier_token(t);
        let next_sig = toks[i + 1..].iter().find(|n| !n.is_trivia());
        let is_call = next_sig.is_some_and(|n| n.kind == TokKind::LParen);
        if is_name
            && !is_call
            && let Some(v) = t.ident()
            && let Some((_, new)) = renames.iter().find(|(old, _)| old.eq_ignore_ascii_case(&v))
        {
            out.push_str(&quote(new));
            continue;
        }
        out.push_str(t.text);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = r#"CREATE TABLE "order items" (
  id INTEGER PRIMARY KEY AUTOINCREMENT, -- surrogate key
  order_id INT NOT NULL REFERENCES orders (id) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED,
  sku VARCHAR(32) COLLATE NOCASE CONSTRAINT uq_sku UNIQUE ON CONFLICT REPLACE,
  qty INTEGER DEFAULT 1 CHECK (qty > 0),
  price NUMERIC(10, 2) DEFAULT (0.0),
  note TEXT DEFAULT 'n/a',
  total REAL GENERATED ALWAYS AS (qty * price) STORED,
  half AS (price / 2),
  neg INT DEFAULT -1,
  CONSTRAINT ck_price CHECK (price >= 0),
  UNIQUE (order_id, sku),
  CONSTRAINT fk_x FOREIGN KEY (order_id, sku) REFERENCES other (a, b) ON UPDATE SET NULL
) WITHOUT ROWID, STRICT"#;

    #[test]
    fn parses_table() {
        let t = parse_create_table(TABLE).expect("parse");
        assert_eq!(t.name, "order items");
        assert!(t.without_rowid && t.strict);
        assert_eq!(t.columns.len(), 9);
        let id = &t.columns[0];
        assert_eq!(id.type_name, "INTEGER");
        assert!(id.primary_key.as_ref().unwrap().autoincrement);
        assert_eq!(id.comment.as_deref(), Some("surrogate key"));
        let order_id = &t.columns[1];
        assert!(order_id.not_null.is_some());
        assert_eq!(order_id.references[0].ref_table, "orders");
        assert_eq!(order_id.references[0].on_delete.as_deref(), Some("CASCADE"));
        assert!(order_id.references[0].deferred);
        let sku = t.column("SKU").unwrap();
        assert_eq!(sku.type_name, "VARCHAR(32)");
        assert_eq!(sku.collation.as_deref(), Some("NOCASE"));
        assert_eq!(sku.unique[0].name.as_deref(), Some("uq_sku"));
        assert_eq!(sku.unique[0].conflict.as_deref(), Some("REPLACE"));
        assert_eq!(t.column("qty").unwrap().default.as_deref(), Some("1"));
        assert_eq!(t.column("qty").unwrap().checks[0].expression, "qty > 0");
        assert_eq!(t.column("price").unwrap().type_name, "NUMERIC(10, 2)");
        assert_eq!(t.column("price").unwrap().default.as_deref(), Some("0.0"));
        assert_eq!(t.column("note").unwrap().default.as_deref(), Some("'n/a'"));
        assert_eq!(
            t.column("total").unwrap().generated,
            Some(("qty * price".to_string(), true))
        );
        assert_eq!(
            t.column("half").unwrap().generated,
            Some(("price / 2".to_string(), false))
        );
        assert_eq!(t.column("half").unwrap().type_name, "");
        assert_eq!(t.column("neg").unwrap().default.as_deref(), Some("-1"));
        let checks = t.checks();
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[1].name.as_deref(), Some("ck_price"));
        let fks = t.foreign_keys();
        assert_eq!(fks.len(), 2);
        assert_eq!(fks[1].name.as_deref(), Some("fk_x"));
        assert_eq!(fks[1].columns, vec!["order_id", "sku"]);
        assert_eq!(fks[1].ref_columns, vec!["a", "b"]);
        assert_eq!(fks[1].on_update.as_deref(), Some("SET NULL"));
        assert_eq!(t.autoincrement_column().as_deref(), Some("id"));
        assert_eq!(t.unique_constraints().len(), 2);
    }

    #[test]
    fn parses_table_level_pk() {
        let t = parse_create_table(
            "CREATE TABLE t(a, b TEXT, CONSTRAINT pk PRIMARY KEY (a DESC, b) ON CONFLICT IGNORE)",
        )
        .unwrap();
        assert_eq!(t.primary_key_columns(), vec!["a", "b"]);
        let info = t.primary_key_info().unwrap();
        assert_eq!(info.name.as_deref(), Some("pk"));
        assert_eq!(info.conflict.as_deref(), Some("IGNORE"));
        assert_eq!(t.columns[0].type_name, "");
        let auto = parse_create_table("CREATE TABLE t(id INTEGER, PRIMARY KEY(id AUTOINCREMENT))")
            .unwrap();
        assert_eq!(auto.autoincrement_column().as_deref(), Some("id"));
    }

    #[test]
    fn parses_virtual_table() {
        let t = parse_create_table(
            "CREATE VIRTUAL TABLE docs USING fts5(title, body, tokenize = 'porter')",
        )
        .unwrap();
        assert_eq!(
            t.virtual_module,
            Some(("fts5".into(), "title, body, tokenize = 'porter'".into()))
        );
    }

    #[test]
    fn parses_index() {
        let i = parse_create_index("CREATE UNIQUE INDEX IF NOT EXISTS main.ix ON t (a COLLATE NOCASE DESC, lower(b), \"c d\") WHERE a > 0 AND b IS NOT NULL").unwrap();
        assert!(i.unique);
        assert_eq!(i.name, "ix");
        assert_eq!(i.table, "t");
        assert_eq!(i.columns.len(), 3);
        assert_eq!(i.columns[0].name, "a");
        assert!(i.columns[0].descending);
        assert_eq!(i.columns[0].collation.as_deref(), Some("NOCASE"));
        assert!(i.columns[1].is_expression);
        assert_eq!(i.columns[1].name, "lower(b)");
        assert_eq!(i.columns[2].name, "c d");
        assert_eq!(i.predicate.as_deref(), Some("a > 0 AND b IS NOT NULL"));
    }

    #[test]
    fn parses_trigger() {
        let sql = "CREATE TRIGGER trg_upd AFTER UPDATE OF a, b ON t FOR EACH ROW WHEN NEW.a > 1 BEGIN\n  INSERT INTO log(x) VALUES (NEW.a);\n  UPDATE t SET b = CASE WHEN 1 THEN 2 END;\nEND";
        let t = parse_create_trigger(sql).unwrap();
        assert_eq!(t.name, "trg_upd");
        assert_eq!(t.timing, "AFTER");
        assert_eq!(t.event, "UPDATE OF a, b");
        assert_eq!(t.table, "t");
        assert!(t.for_each_row);
        assert_eq!(t.when.as_deref(), Some("NEW.a > 1"));
        assert!(t.body.starts_with("INSERT INTO log"));
        assert!(t.body.ends_with("END;"), "{}", t.body);
        let i = parse_create_trigger(
            "CREATE TEMP TRIGGER v_ins INSTEAD OF INSERT ON v BEGIN SELECT 1; END",
        )
        .unwrap();
        assert_eq!(i.timing, "INSTEAD OF");
        assert!(i.temporary);
    }

    #[test]
    fn parses_view() {
        let v = parse_create_view("CREATE VIEW v (x, y) AS SELECT a, b FROM t;").unwrap();
        assert_eq!(v.columns, Some(vec!["x".to_string(), "y".to_string()]));
        assert_eq!(v.select, "SELECT a, b FROM t");
        let v = parse_create_view(
            "CREATE TEMP VIEW IF NOT EXISTS w AS\nWITH q AS (SELECT 1) SELECT * FROM q",
        )
        .unwrap();
        assert!(v.temporary);
        assert!(v.select.starts_with("WITH q"));
    }

    #[test]
    fn rewrites_names() {
        let s = rewrite_create(
            "CREATE INDEX ix ON t (a)",
            "INDEX",
            "\"aux\".\"ix2\"",
            Some("\"t2\""),
            false,
        )
        .unwrap();
        assert_eq!(s, "CREATE INDEX \"aux\".\"ix2\" ON \"t2\" (a)");
        let s = rewrite_create(
            "CREATE TEMP TRIGGER tr AFTER INSERT ON t BEGIN SELECT 1; END",
            "TRIGGER",
            "temp.tr",
            Some("t"),
            true,
        )
        .unwrap();
        assert_eq!(
            s,
            "CREATE TRIGGER temp.tr AFTER INSERT ON t BEGIN SELECT 1; END"
        );
        let s = rewrite_create(
            "CREATE VIEW v(a) AS SELECT 1",
            "VIEW",
            "main.v",
            None,
            false,
        )
        .unwrap();
        assert_eq!(s, "CREATE VIEW main.v(a) AS SELECT 1");
    }

    #[test]
    fn identifier_helpers() {
        assert_eq!(
            expression_identifiers("lower(a) > b AND \"c\" = 'x'"),
            vec!["a", "b", "c"]
        );
        let q = |s: &str| format!("\"{s}\"");
        let r = rename_identifiers("a > 0 AND max(a, b) < A", &[("a".into(), "z".into())], &q);
        assert_eq!(r, "\"z\" > 0 AND max(\"z\", b) < \"z\"");
    }
}
