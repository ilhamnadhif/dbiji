//! SQL formatter (FR-S5).
//!
//! The formatter only changes white space and keyword case: every other
//! token — strings, comments, quoted identifiers, dollar-quoted bodies,
//! numbers, operators — is preserved byte for byte and in order. This is
//! enforced, not just intended: the output is re-tokenized and compared with
//! the input, and on any difference the original text is returned. A
//! statement whose full layout fails that check falls back to a light layout
//! (original line structure, keyword case only), so a bug can never change
//! the meaning of a script.
//!
//! Layout: one clause per line (`SELECT`, `FROM`, `JOIN`, `WHERE`, `GROUP
//! BY` ...); lists stay on the clause line when short and
//! [`FormatOptions::compact_short_lists`] is set, otherwise one item per
//! line; conditions break before (or after) `AND` / `OR`; subqueries and CTE
//! bodies are indented; `CREATE TABLE` puts one column per line; function
//! calls stay compact; comments stay attached to their lines. Routine bodies
//! and other procedural code keep their line structure. Formatting is
//! idempotent.

mod doc;
mod layout;
#[cfg(test)]
mod tests;

use crate::KeywordCase;
use crate::lexicon::{WordSet, reserved, word_set};
use crate::split::{Split, SplitOptions, split_tokens};
use crate::token::{Token, TokenKind, delimiter_command_arg, is_word, tokenize};
use dbiji_core::SqlSyntax;
use layout::{FTok, Layout, Note};
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::rc::Rc;

/// Indentation unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndentStyle {
    /// That many spaces.
    Spaces(u8),
    /// One tab per level.
    Tab,
}

impl Default for IndentStyle {
    fn default() -> Self {
        IndentStyle::Spaces(4)
    }
}

/// Formatter settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FormatOptions {
    /// Case of keywords.
    pub keyword_case: KeywordCase,
    /// Indentation unit.
    pub indent: IndentStyle,
    /// Put commas at the start of continuation lines.
    pub comma_first: bool,
    /// Break before `AND` / `OR` (otherwise after them).
    pub newline_before_and_or: bool,
    /// Preferred maximum line width.
    pub max_line_length: usize,
    /// Blank lines between statements.
    pub lines_between_statements: usize,
    /// Keep short lists on one line (`SELECT a, b FROM t`).
    pub compact_short_lists: bool,
}

impl Default for FormatOptions {
    fn default() -> Self {
        FormatOptions {
            keyword_case: KeywordCase::Upper,
            indent: IndentStyle::Spaces(4),
            comma_first: false,
            newline_before_and_or: true,
            max_line_length: 100,
            lines_between_statements: 1,
            compact_short_lists: true,
        }
    }
}

/// Non-reserved keywords whose case is changed (besides SQL:2016 reserved
/// words). Other keywords keep their case because they are often used as
/// identifiers.
const CASE_WORDS: &[&str] = &[
    "ADD",
    "AFTER",
    "ANALYZE",
    "ASC",
    "ATOMIC",
    "BEFORE",
    "BEGIN",
    "CALL",
    "CASCADE",
    "COMMIT",
    "CONCURRENTLY",
    "CONFLICT",
    "CURSOR",
    "DATABASE",
    "DEALLOCATE",
    "DECLARE",
    "DEFERRED",
    "DELAYED",
    "DESCRIBE",
    "DISTINCTROW",
    "DUPLICATE",
    "EACH",
    "ELSEIF",
    "ESCAPE",
    "EXCLUDE",
    "EXECUTE",
    "EXPLAIN",
    "EXTENSION",
    "FILTER",
    "FOLLOWING",
    "FUNCTION",
    "IF",
    "IGNORE",
    "ILIKE",
    "IMMEDIATE",
    "INDEX",
    "INOUT",
    "INTERVAL",
    "ISOLATION",
    "LANGUAGE",
    "LOCK",
    "LOOP",
    "MATCHED",
    "MATERIALIZED",
    "MERGE",
    "MODIFY",
    "NOTHING",
    "NOWAIT",
    "NULLS",
    "OVER",
    "PARTITION",
    "PRAGMA",
    "PRECEDING",
    "PROCEDURE",
    "RECURSIVE",
    "REFRESH",
    "REINDEX",
    "RELEASE",
    "RENAME",
    "REPEAT",
    "REPLACE",
    "RESTRICT",
    "RETURN",
    "RETURNS",
    "ROLLBACK",
    "SAVEPOINT",
    "SCHEMA",
    "SEQUENCE",
    "SHOW",
    "STRAIGHT_JOIN",
    "TEMPORARY",
    "TIES",
    "TRANSACTION",
    "TRIGGER",
    "TRUNCATE",
    "UNBOUNDED",
    "UNLOCK",
    "UNLOGGED",
    "UNTIL",
    "USE",
    "VACUUM",
    "VIEW",
    "WHILE",
    "WITHIN",
    "WITHOUT",
    "WORK",
    "OUTER",
    "INNER",
    "PRIMARY",
    "EXISTS",
    "KEY",
];
/// After these words a keyword is probably used as a name (`FROM user`).
const NAME_POSITION: &[&str] = &["FROM", "JOIN", "INTO", "UPDATE", "TABLE", "AS"];
/// Keywords that are never names, even in name positions.
const NEVER_NAMES: &[&str] = &[
    "SELECT",
    "WITH",
    "VALUES",
    "LATERAL",
    "ONLY",
    "NOT",
    "MATERIALIZED",
    "RECURSIVE",
    "IGNORE",
    "TEMPORARY",
    "IF",
    "OR",
    "DISTINCT",
    "ALL",
    "DELAYED",
    "LOW_PRIORITY",
    "HIGH_PRIORITY",
    "SET",
];
/// Statement heads formatted with their original line structure.
const PROCEDURAL_HEADS: &[&str] = &[
    "DECLARE", "IF", "WHILE", "LOOP", "REPEAT", "FOR", "CASE", "DO",
];
const ROUTINE_WORDS: &[&str] = &[
    "PROCEDURE",
    "FUNCTION",
    "TRIGGER",
    "EVENT",
    "PACKAGE",
    "PROC",
    "RULE",
];

/// Format a script.
pub fn format_sql(src: &str, syntax: &SqlSyntax, opts: &FormatOptions) -> String {
    let (out, _) = format_inner(src, syntax, opts);
    if equivalent(src, &out, syntax) {
        out
    } else {
        src.to_string()
    }
}

/// Formatted text without the final safety check, and the number of
/// statements whose full layout had to fall back to the light layout.
pub(crate) fn format_inner(src: &str, syntax: &SqlSyntax, opts: &FormatOptions) -> (String, usize) {
    if src.trim().is_empty() {
        return (src.to_string(), 0);
    }
    let split = split_tokens(src, syntax, &SplitOptions::default());
    let f = Fmt::new(src, syntax, opts, &split);
    let out = f.run();
    (out, f.fallbacks.get())
}

/// Same tokens apart from white space and keyword case.
fn equivalent(a: &str, b: &str, syntax: &SqlSyntax) -> bool {
    let ta: Vec<Token> = tokenize(a, syntax)
        .into_iter()
        .filter(|t| !matches!(t.kind, TokenKind::Whitespace | TokenKind::Newline))
        .collect();
    let tb: Vec<Token> = tokenize(b, syntax)
        .into_iter()
        .filter(|t| !matches!(t.kind, TokenKind::Whitespace | TokenKind::Newline))
        .collect();
    same_tokens(a, &ta, b, &tb)
}

fn same_tokens(a: &str, ta: &[Token], b: &str, tb: &[Token]) -> bool {
    ta.len() == tb.len()
        && ta.iter().zip(tb).all(|(x, y)| {
            let (sx, sy) = (x.text(a), y.text(b));
            x.kind == y.kind
                && (sx == sy || (x.kind == TokenKind::Keyword && sx.eq_ignore_ascii_case(sy)))
        })
}

enum Unit {
    Stmt {
        idx: usize,
        lead: Vec<usize>,
        pre: Vec<usize>,
        term: Option<usize>,
        trail: Vec<usize>,
    },
    Comments(Vec<usize>),
    Verbatim(usize),
}

struct Fmt<'a> {
    src: &'a str,
    syntax: &'a SqlSyntax,
    opts: &'a FormatOptions,
    split: &'a Split,
    reserved: Rc<WordSet>,
    extra: Rc<WordSet>,
    unit: String,
    unit_width: usize,
    fallbacks: Cell<usize>,
}

impl<'a> Fmt<'a> {
    fn new(
        src: &'a str,
        syntax: &'a SqlSyntax,
        opts: &'a FormatOptions,
        split: &'a Split,
    ) -> Fmt<'a> {
        let (unit, unit_width) = match opts.indent {
            IndentStyle::Spaces(n) => (" ".repeat(n as usize), n as usize),
            IndentStyle::Tab => ("\t".to_string(), 4),
        };
        Fmt {
            src,
            syntax,
            opts,
            split,
            reserved: reserved(),
            extra: word_set(CASE_WORDS, false),
            unit,
            unit_width,
            fallbacks: Cell::new(0),
        }
    }

    fn keyword_text(&self, text: &str, prev_upper: &str) -> String {
        if self.opts.keyword_case == KeywordCase::Preserve {
            return text.to_string();
        }
        // `NULLS FIRST` / `NULLS LAST`: always keywords there.
        let after_nulls = prev_upper == "NULLS"
            && (text.eq_ignore_ascii_case("first") || text.eq_ignore_ascii_case("last"));
        if after_nulls {
            return self.opts.keyword_case.apply(text);
        }
        if !(self.reserved.contains(text) || self.extra.contains(text)) {
            return text.to_string();
        }
        let upper = text.to_ascii_uppercase();
        if NAME_POSITION.contains(&prev_upper) && !NEVER_NAMES.contains(&upper.as_str()) {
            return text.to_string();
        }
        self.opts.keyword_case.apply(text)
    }

    fn run(&self) -> String {
        let units = self.units();
        let mut out = String::with_capacity(self.src.len() + 64);
        let mut prev_unterminated = false;
        for (k, u) in units.iter().enumerate() {
            let (text, unterminated) = match u {
                Unit::Stmt {
                    idx,
                    lead,
                    pre,
                    term,
                    trail,
                } => {
                    let st = &self.split.statements[*idx];
                    (
                        self.render_stmt(*idx, lead, pre, *term, trail),
                        !st.span.terminated && !st.span.is_client_command,
                    )
                }
                Unit::Comments(c) => (
                    c.iter()
                        .map(|&i| self.split.tokens[i].text(self.src))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    false,
                ),
                Unit::Verbatim(i) => (self.split.tokens[*i].text(self.src).to_string(), false),
            };
            if k > 0 {
                let mut blank = self.opts.lines_between_statements;
                if prev_unterminated {
                    blank = blank.max(1);
                }
                out.push('\n');
                for _ in 0..blank {
                    out.push('\n');
                }
            }
            out.push_str(&text);
            prev_unterminated = unterminated;
        }
        if self.src.ends_with('\n') && !out.is_empty() {
            out.push('\n');
        }
        out
    }

    /// Group the script tokens into statements, comment blocks and command lines.
    fn units(&self) -> Vec<Unit> {
        let toks = &self.split.tokens;
        let stmts = &self.split.statements;
        let n = toks.len();
        let mut units = Vec::new();
        let mut pending: Vec<usize> = Vec::new();
        let mut blank_mark = 0usize;
        let mut newlines = 0u32;
        let mut si = 0;
        let mut i = 0;
        let flush = |pending: &mut Vec<usize>, units: &mut Vec<Unit>| {
            if !pending.is_empty() {
                units.push(Unit::Comments(std::mem::take(pending)));
            }
        };
        while i < n {
            while si < stmts.len() && stmts[si].tokens.start < i {
                si += 1; // statements overlapping consumed tokens (should not happen)
            }
            if si < stmts.len() && i == stmts[si].tokens.start {
                let st = &stmts[si];
                let attached = pending.split_off(blank_mark.min(pending.len()));
                flush(&mut pending, &mut units);
                let end = st.tokens.end.min(n);
                let mut j = end;
                let mut pre = Vec::new();
                let mut term = None;
                if st.span.terminated && !st.span.is_client_command {
                    let mut k = end;
                    while k < n {
                        match toks[k].kind {
                            TokenKind::Whitespace | TokenKind::Newline => k += 1,
                            TokenKind::LineComment | TokenKind::BlockComment => {
                                pre.push(k);
                                k += 1;
                            }
                            TokenKind::Semicolon | TokenKind::Delimiter
                                if toks[k].end == st.span.outer_end =>
                            {
                                term = Some(k);
                                k += 1;
                                break;
                            }
                            _ => break,
                        }
                    }
                    if term.is_some() {
                        j = k;
                    } else {
                        pre.clear();
                    }
                }
                // Comments on the same line after the statement.
                let mut trail = Vec::new();
                let mut k = j;
                while k < n {
                    match toks[k].kind {
                        TokenKind::Whitespace => k += 1,
                        TokenKind::LineComment | TokenKind::BlockComment => {
                            trail.push(k);
                            k += 1;
                        }
                        _ => break,
                    }
                }
                if !trail.is_empty() {
                    j = k;
                }
                units.push(Unit::Stmt {
                    idx: si,
                    lead: attached,
                    pre,
                    term,
                    trail,
                });
                si += 1;
                i = j;
                blank_mark = 0;
                newlines = 0;
                continue;
            }
            let t = toks[i];
            match t.kind {
                TokenKind::Whitespace => {}
                TokenKind::Newline => {
                    newlines += 1;
                    if newlines >= 2 {
                        blank_mark = pending.len();
                    }
                }
                TokenKind::LineComment | TokenKind::BlockComment => {
                    pending.push(i);
                    newlines = 0;
                }
                _ => {
                    // DELIMITER commands, stray `;` / `GO`: kept on their own line.
                    flush(&mut pending, &mut units);
                    blank_mark = 0;
                    newlines = 0;
                    units.push(Unit::Verbatim(i));
                }
            }
            i += 1;
        }
        flush(&mut pending, &mut units);
        units
    }

    fn is_batch_separator(&self, t: &Token) -> bool {
        t.kind == TokenKind::Delimiter
            && self.syntax.batch_separator.is_some_and(|b| {
                t.text(self.src)
                    .get(..b.len())
                    .is_some_and(|h| h.eq_ignore_ascii_case(b))
            })
            && delimiter_command_arg(t, self.src).is_none()
    }

    fn render_stmt(
        &self,
        idx: usize,
        lead: &[usize],
        pre: &[usize],
        term: Option<usize>,
        trail: &[usize],
    ) -> String {
        let toks = &self.split.tokens;
        let st = &self.split.statements[idx];
        let mut s = String::new();
        for &c in lead {
            s.push_str(toks[c].text(self.src));
            s.push('\n');
        }
        s.push_str(&self.body(idx));
        let mut last_line_comment = false;
        let mut prev_end = st.span.end;
        for &c in pre {
            let between = self.src.get(prev_end..toks[c].start).unwrap_or("");
            if last_line_comment || between.contains(['\n', '\r']) {
                s.push('\n');
            } else {
                s.push(' ');
            }
            s.push_str(toks[c].text(self.src));
            last_line_comment = toks[c].kind == TokenKind::LineComment;
            prev_end = toks[c].end;
        }
        if let Some(t) = term {
            let tok = &toks[t];
            if last_line_comment || self.is_batch_separator(tok) {
                s.push('\n');
            } else if tok.kind == TokenKind::Delimiter && tok.start > prev_end {
                s.push(' ');
            }
            s.push_str(tok.text(self.src));
        }
        for &c in trail {
            s.push(' ');
            s.push_str(toks[c].text(self.src));
        }
        s
    }

    /// Formatted text of statement `idx` (without terminator).
    fn body(&self, idx: usize) -> String {
        let st = &self.split.statements[idx];
        let toks = self.split.tokens.get(st.tokens.clone()).unwrap_or(&[]);
        if st.span.is_client_command || toks.iter().all(Token::is_trivia) {
            // Client commands and executable comments (`/*!40101 ... */`) are kept verbatim.
            return st.span.text(self.src).to_string();
        }
        let original: Vec<Token> = toks
            .iter()
            .filter(|t| !matches!(t.kind, TokenKind::Whitespace | TokenKind::Newline))
            .copied()
            .collect();
        let check = |text: &str| {
            let tb: Vec<Token> = tokenize(text, self.syntax)
                .into_iter()
                .filter(|t| !matches!(t.kind, TokenKind::Whitespace | TokenKind::Newline))
                .collect();
            same_tokens(self.src, &original, text, &tb)
        };
        if !self.procedural(toks) {
            let full = self.full(toks);
            if check(&full) {
                return full;
            }
            self.fallbacks.set(self.fallbacks.get() + 1);
            tracing::debug!(
                "formatter: full layout changed the tokens of a statement; using the light layout"
            );
        }
        let light = self.light(toks);
        if check(&light) {
            return light;
        }
        st.span.text(self.src).to_string()
    }

    /// Routine definitions and procedural blocks keep their line structure.
    fn procedural(&self, toks: &[Token]) -> bool {
        let sig: Vec<&Token> = toks.iter().filter(|t| !t.is_trivia()).take(16).collect();
        let Some(first) = sig.first() else {
            return false;
        };
        if PROCEDURAL_HEADS.iter().any(|w| is_word(first, self.src, w)) {
            return true;
        }
        if is_word(first, self.src, "BEGIN") && sig.len() > 1 {
            return true;
        }
        if is_word(first, self.src, "CREATE") || is_word(first, self.src, "ALTER") {
            return sig
                .iter()
                .skip(1)
                .take(10)
                .any(|t| ROUTINE_WORDS.iter().any(|w| is_word(t, self.src, w)));
        }
        false
    }

    fn prepare(&self, toks: &[Token]) -> Vec<FTok> {
        let mut out: Vec<FTok> = Vec::new();
        let mut pending: Vec<Note> = Vec::new();
        let mut ws = false;
        let mut nl = false;
        let mut last_was_note = false;
        let mut prev_upper = String::new();
        for t in toks {
            match t.kind {
                TokenKind::Whitespace => ws = true,
                TokenKind::Newline => {
                    ws = true;
                    nl = true;
                    if last_was_note {
                        let note = match pending.last_mut() {
                            Some(n) => Some(n),
                            None => out.last_mut().and_then(|o| o.trail.last_mut()),
                        };
                        if let Some(n) = note {
                            n.nl_after = true;
                        }
                    }
                }
                TokenKind::LineComment | TokenKind::BlockComment => {
                    let note = Note {
                        text: t.text(self.src).to_string(),
                        line: t.kind == TokenKind::LineComment,
                        nl_before: nl,
                        nl_after: false,
                    };
                    match out.last_mut() {
                        Some(o) if !nl && pending.is_empty() => o.trail.push(note),
                        _ => pending.push(note),
                    }
                    nl = false;
                    ws = true;
                    last_was_note = true;
                }
                _ => {
                    let text = t.text(self.src);
                    let upper = if matches!(t.kind, TokenKind::Keyword | TokenKind::Identifier) {
                        text.to_ascii_uppercase()
                    } else {
                        String::new()
                    };
                    let out_text = if t.kind == TokenKind::Keyword {
                        self.keyword_text(text, &prev_upper)
                    } else {
                        text.to_string()
                    };
                    let glued = !ws && !out.is_empty();
                    out.push(FTok {
                        kind: t.kind,
                        text: out_text,
                        upper: upper.clone(),
                        lead: std::mem::take(&mut pending),
                        trail: Vec::new(),
                        glued,
                    });
                    prev_upper = upper;
                    ws = false;
                    nl = false;
                    last_was_note = false;
                }
            }
        }
        if let Some(o) = out.last_mut() {
            o.trail.append(&mut pending);
        }
        out
    }

    fn full(&self, toks: &[Token]) -> String {
        let ftoks = self.prepare(toks);
        let doc = Layout::new(&ftoks, self.opts).layout();
        doc::render(&doc, &self.unit, self.unit_width, self.opts.max_line_length)
    }

    /// Original line structure; keyword case applied, trailing spaces removed.
    fn light(&self, toks: &[Token]) -> String {
        let mut s = String::new();
        let mut prev_upper = String::new();
        for (k, t) in toks.iter().enumerate() {
            match t.kind {
                TokenKind::Whitespace => {
                    let next_nl = toks.get(k + 1).is_none_or(|n| n.kind == TokenKind::Newline);
                    if !next_nl {
                        s.push_str(t.text(self.src));
                    }
                }
                TokenKind::Newline => s.push('\n'),
                TokenKind::Keyword => {
                    s.push_str(&self.keyword_text(t.text(self.src), &prev_upper));
                }
                _ => s.push_str(t.text(self.src)),
            }
            if !t.is_trivia() {
                prev_upper = if matches!(t.kind, TokenKind::Keyword | TokenKind::Identifier) {
                    t.text(self.src).to_ascii_uppercase()
                } else {
                    String::new()
                };
            }
        }
        s
    }
}
