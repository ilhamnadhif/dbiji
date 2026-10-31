//! Splitting scripts into statements.
//!
//! Statements end at the delimiter (`;` by default) outside strings,
//! comments and dollar quotes, at MySQL custom delimiters set with
//! `DELIMITER xx`, at batch separator lines (`GO`) and, depending on
//! [`BlankLineMode`], at blank lines. Client commands (`@set ...`) are
//! single-line statements of their own.
//!
//! Routine bodies are kept intact: with [`SqlSyntax::compound_blocks`],
//! `BEGIN ... END`, `CASE ... END`, `IF ... END IF`, `LOOP ... END LOOP`,
//! `WHILE ... END WHILE`, `REPEAT ... END REPEAT` inside
//! `CREATE [OR REPLACE] [DEFINER=..] PROCEDURE|FUNCTION|TRIGGER|EVENT` do not
//! split on inner `;` (SQLite trigger bodies too), and for every dialect the
//! SQL-standard `BEGIN ATOMIC ... END` function bodies stay whole. Plain
//! `BEGIN;` / `BEGIN TRANSACTION;` / `BEGIN WORK;` remain normal statements.

use crate::position::floor_char_boundary;
use crate::token::{Token, TokenKind, delimiter_command_arg, is_word, tokenize_with_delimiter};
use dbiji_core::SqlSyntax;
use serde::{Deserialize, Serialize};
use std::ops::Range;

/// Whether a blank line ends a statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BlankLineMode {
    /// Blank lines never end statements.
    Never,
    /// A blank line ends a statement unless the statement obviously
    /// continues: inside parentheses or blocks, after a comma, operator or a
    /// keyword such as `FROM`/`AND`, or before a line starting with `WHERE`,
    /// `JOIN`, `ORDER BY` ... (strings and comments are never split).
    #[default]
    Smart,
    /// Every blank line outside strings, comments and routine bodies ends a statement.
    Always,
}

/// Statement splitting options (user settings).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SplitOptions {
    /// Statement delimiter; `;` by default. `DELIMITER xx` commands in MySQL
    /// scripts override it until changed back.
    pub delimiter: String,
    /// When blank lines end statements.
    pub blank_line: BlankLineMode,
    /// Treat `@...` lines at statement start as client commands.
    pub client_commands: bool,
}

impl Default for SplitOptions {
    fn default() -> Self {
        SplitOptions {
            delimiter: ";".into(),
            blank_line: BlankLineMode::Smart,
            client_commands: true,
        }
    }
}

/// One statement of a script.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StatementSpan {
    /// Start of the statement text: its first significant token (MySQL
    /// executable comments `/*!...*/` count as significant when they form
    /// the whole statement).
    pub start: usize,
    /// End of the statement text: end of its last significant token. The
    /// delimiter, trailing whitespace and trailing comments are excluded.
    pub end: usize,
    /// Start including comments directly attached above the statement (no
    /// blank line in between).
    pub outer_start: usize,
    /// End including the terminating delimiter (equals `end` when the
    /// statement is not terminated).
    pub outer_end: usize,
    /// The statement was ended by a delimiter / batch separator.
    pub terminated: bool,
    /// The statement is a client command line (`@set`, `@include` ...).
    pub is_client_command: bool,
}

impl StatementSpan {
    /// Statement text `src[start..end]`.
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        src.get(self.start..self.end).unwrap_or("")
    }

    /// `start <= offset <= end` (the end is inclusive so that a cursor
    /// right after the last character belongs to the statement).
    pub fn contains(&self, offset: usize) -> bool {
        self.start <= offset && offset <= self.end
    }

    /// Classification of the statement (see [`crate::classify::classify`];
    /// [`crate::classify::split_and_classify`] avoids re-tokenizing).
    pub fn info(&self, src: &str, syntax: &SqlSyntax) -> crate::classify::StatementInfo {
        crate::classify::classify(self.text(src), syntax)
    }
}

/// A statement together with the range of token indices it covers.
#[derive(Debug, Clone)]
pub(crate) struct SplitStatement {
    pub span: StatementSpan,
    /// Indices into [`Split::tokens`]: first significant token up to and
    /// including the last one.
    pub tokens: Range<usize>,
}

/// Tokens of a script and its statements.
#[derive(Debug, Clone, Default)]
pub(crate) struct Split {
    pub tokens: Vec<Token>,
    pub statements: Vec<SplitStatement>,
}

/// Split a script into statements.
pub fn split_statements(src: &str, syntax: &SqlSyntax, opts: &SplitOptions) -> Vec<StatementSpan> {
    split_tokens(src, syntax, opts)
        .statements
        .into_iter()
        .map(|s| s.span)
        .collect()
}

/// The statement to execute for a cursor at `offset` (DBeaver semantics):
/// the statement containing the cursor; else the one ending before the
/// cursor on the same line (cursor after the delimiter or a trailing
/// comment); else the statement whose attached comments or indentation the
/// cursor is in; else the statement ending on the previous line when the
/// cursor is on a blank line right below it. `None` when the cursor is
/// between statements.
pub fn statement_at(
    src: &str,
    offset: usize,
    syntax: &SqlSyntax,
    opts: &SplitOptions,
) -> Option<StatementSpan> {
    let offset = floor_char_boundary(src, offset);
    let split = split_tokens(src, syntax, opts);
    let spans: Vec<StatementSpan> = split.statements.iter().map(|s| s.span).collect();
    pick_statement(&spans, src, offset)
}

pub(crate) fn pick_statement(
    spans: &[StatementSpan],
    src: &str,
    offset: usize,
) -> Option<StatementSpan> {
    if let Some(s) = spans.iter().find(|s| s.contains(offset)) {
        return Some(*s);
    }
    let prev = spans.iter().rev().find(|s| s.end < offset);
    let next = spans.iter().find(|s| s.start > offset);
    if let Some(p) = prev {
        let between = src.get(p.end..offset).unwrap_or("");
        if !between.contains(['\n', '\r']) {
            return Some(*p);
        }
    }
    if let Some(n) = next {
        if offset >= n.outer_start {
            return Some(*n);
        }
        // Indentation before the statement (or before its attached comments).
        let between = src.get(offset..n.outer_start).unwrap_or("");
        if between.chars().all(|c| c == ' ' || c == '\t') {
            return Some(*n);
        }
    }
    if let Some(p) = prev {
        let between = src.get(p.end..offset).unwrap_or("");
        let breaks = between
            .matches('\n')
            .count()
            .max(between.matches('\r').count());
        let line_end = src[offset..]
            .find(['\n', '\r'])
            .map(|i| offset + i)
            .unwrap_or(src.len());
        let line_start = src[..offset]
            .rfind(['\n', '\r'])
            .map(|i| i + 1)
            .unwrap_or(0);
        let line_blank = src[line_start..line_end].trim().is_empty();
        if breaks == 1 && line_blank {
            return Some(*p);
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockMode {
    /// Not decided yet (still reading the statement header).
    Undecided,
    /// Ordinary statement: `;` always splits.
    Plain,
    /// Routine with compound statements (`BEGIN ... END` protects `;`).
    Compound,
    /// T-SQL routine: the body runs to the end of the batch (`GO`).
    UntilBatchEnd,
    /// Only SQL-standard `BEGIN ATOMIC ... END` bodies protect `;`.
    Atomic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Header {
    Start,
    AfterCreate { definer: bool },
    Done,
}

struct Seg {
    first_sig: Option<usize>,
    last_sig: usize,
    leading_comment: Option<usize>,
    exec_comment: Option<(usize, usize)>,
    after_newline: bool,
    paren: i32,
    blocks: u32,
    mode: BlockMode,
    header: Header,
    header_len: u32,
    skip_end_word: bool,
    /// A PL/SQL `DECLARE` opened the block its `BEGIN` belongs to.
    declare_open: bool,
}

impl Seg {
    fn new(after_newline: bool) -> Seg {
        Seg {
            first_sig: None,
            last_sig: 0,
            leading_comment: None,
            exec_comment: None,
            after_newline,
            paren: 0,
            blocks: 0,
            mode: BlockMode::Undecided,
            header: Header::Start,
            header_len: 0,
            skip_end_word: false,
            declare_open: false,
        }
    }

    fn has_content(&self) -> bool {
        self.first_sig.is_some() || self.exec_comment.is_some()
    }

    fn is_routine(&self) -> bool {
        matches!(
            self.mode,
            BlockMode::Compound | BlockMode::UntilBatchEnd | BlockMode::Atomic
        )
    }
}

const ROUTINE_WORDS: &[&str] = &[
    "PROCEDURE",
    "FUNCTION",
    "TRIGGER",
    "EVENT",
    "PROC",
    "PACKAGE",
];
/// PL/SQL object types whose bodies end at a `/` line.
const PLSQL_ROUTINE_WORDS: &[&str] = &["TYPE", "LIBRARY", "JAVA"];
const HEADER_FILLERS: &[&str] = &[
    "OR",
    "REPLACE",
    "ALTER",
    "DEFINER",
    "SQL",
    "SECURITY",
    "INVOKER",
    "AGGREGATE",
    "TEMP",
    "TEMPORARY",
    "NONEDITIONABLE",
    "EDITIONABLE",
    "CURRENT_USER",
    "CONSTRAINT",
];
const TX_AFTER_BEGIN: &[&str] = &[
    "TRANSACTION",
    "TRAN",
    "WORK",
    "DISTRIBUTED",
    "DEFERRED",
    "IMMEDIATE",
    "EXCLUSIVE",
    "ISOLATION",
    "READ",
];
const END_SUFFIXES: &[&str] = &[
    "IF", "LOOP", "WHILE", "REPEAT", "CASE", "FOR", "TRY", "CATCH",
];
/// A statement ending with one of these words continues on the next line.
const CONTINUE_AFTER: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "AND",
    "OR",
    "NOT",
    "JOIN",
    "ON",
    "BY",
    "AS",
    "SET",
    "VALUES",
    "INTO",
    "UPDATE",
    "TABLE",
    "WITH",
    "UNION",
    "INTERSECT",
    "EXCEPT",
    "ALL",
    "DISTINCT",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "IN",
    "IS",
    "LIKE",
    "ILIKE",
    "BETWEEN",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "EXISTS",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "OUTER",
    "CROSS",
    "NATURAL",
    "USING",
    "RETURNING",
    "DELETE",
    "INSERT",
    "ORDER",
    "GROUP",
    "PARTITION",
    "OVER",
    "WINDOW",
];
/// A line starting with one of these words continues the previous statement.
const CONTINUE_BEFORE: &[&str] = &[
    "FROM",
    "WHERE",
    "AND",
    "OR",
    "NOT",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "CROSS",
    "OUTER",
    "NATURAL",
    "ON",
    "USING",
    "GROUP",
    "ORDER",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "FETCH",
    "UNION",
    "INTERSECT",
    "EXCEPT",
    "MINUS",
    "WINDOW",
    "RETURNING",
    "THEN",
    "ELSE",
    "END",
    "WHEN",
    "AS",
    "BY",
    "ASC",
    "DESC",
    "LIKE",
    "ILIKE",
    "IN",
    "IS",
    "BETWEEN",
    "QUALIFY",
    "VALUES",
];

fn word_in(t: &Token, src: &str, list: &[&str]) -> bool {
    matches!(t.kind, TokenKind::Keyword | TokenKind::Identifier)
        && list.iter().any(|w| is_word(t, src, w))
}

/// MySQL executable comment `/*! ... */`, sent to the server as a statement.
fn is_exec_comment(t: &Token, src: &str, syntax: &SqlSyntax) -> bool {
    t.kind == TokenKind::BlockComment
        && (syntax.delimiter_command || syntax.hash_comments)
        && t.text(src).starts_with("/*!")
}

struct Splitter<'a> {
    src: &'a str,
    syntax: &'a SqlSyntax,
    opts: &'a SplitOptions,
    tokens: Vec<Token>,
    out: Vec<SplitStatement>,
    custom_delim: bool,
}

/// Tokenize and split; the token vector is returned for reuse.
pub(crate) fn split_tokens(src: &str, syntax: &SqlSyntax, opts: &SplitOptions) -> Split {
    let tokens = tokenize_with_delimiter(src, syntax, &opts.delimiter);
    let d = opts.delimiter.trim();
    let mut sp = Splitter {
        src,
        syntax,
        opts,
        tokens,
        out: Vec::new(),
        custom_delim: !d.is_empty() && d != ";",
    };
    sp.run();
    Split {
        tokens: sp.tokens,
        statements: sp.out,
    }
}

impl Splitter<'_> {
    fn next_sig(&self, i: usize) -> Option<usize> {
        (i + 1..self.tokens.len()).find(|&j| !self.tokens[j].is_trivia())
    }

    fn prev_sig(&self, i: usize) -> Option<usize> {
        (0..i).rev().find(|&j| !self.tokens[j].is_trivia())
    }

    /// Token `i` starts a statement of a routine body (after `;`, BEGIN, THEN, ELSE, DO ...),
    /// or the body itself (`FOR EACH ROW IF …`, `… DETERMINISTIC IF …`, `READS SQL DATA IF …`).
    fn statement_position(&self, i: usize) -> bool {
        match self.prev_sig(i) {
            None => true,
            Some(j) => {
                matches!(
                    self.tokens[j].kind,
                    TokenKind::Semicolon | TokenKind::Delimiter
                ) || [
                    "BEGIN",
                    "THEN",
                    "ELSE",
                    "DO",
                    "LOOP",
                    "REPEAT",
                    "ROW",
                    "DETERMINISTIC",
                    "SQL",
                    "DATA",
                    "DEFINER",
                    "INVOKER",
                ]
                .iter()
                .any(|w| self.word(j, w))
            }
        }
    }

    fn word(&self, i: usize, w: &str) -> bool {
        self.tokens.get(i).is_some_and(|t| is_word(t, self.src, w))
    }

    fn run(&mut self) {
        let mut seg = Seg::new(true);
        let mut pending_newlines = 0u32;
        let n = self.tokens.len();
        let mut i = 0;
        while i < n {
            let t = self.tokens[i];
            match t.kind {
                TokenKind::Whitespace => {}
                TokenKind::Newline => {
                    if pending_newlines >= 1 {
                        self.blank_line(&mut seg, i);
                    }
                    pending_newlines += 1;
                    seg.after_newline = true;
                }
                TokenKind::LineComment | TokenKind::BlockComment => {
                    pending_newlines = 0;
                    if seg.first_sig.is_none() {
                        if is_exec_comment(&t, self.src, self.syntax) {
                            seg.exec_comment =
                                Some((seg.exec_comment.map(|e| e.0).unwrap_or(i), i));
                        }
                        if seg.after_newline && seg.leading_comment.is_none() {
                            seg.leading_comment = Some(t.start);
                        }
                    }
                }
                TokenKind::ClientCommand if self.opts.client_commands => {
                    pending_newlines = 0;
                    let lead = if seg.first_sig.is_none() {
                        seg.leading_comment
                    } else {
                        None
                    };
                    self.finish(&seg, None);
                    let end = t.start + t.text(self.src).trim_end().len();
                    self.out.push(SplitStatement {
                        span: StatementSpan {
                            start: t.start,
                            end,
                            outer_start: lead.unwrap_or(t.start).min(t.start),
                            outer_end: end,
                            terminated: false,
                            is_client_command: true,
                        },
                        tokens: i..i + 1,
                    });
                    seg = Seg::new(false);
                }
                TokenKind::Delimiter => {
                    pending_newlines = 0;
                    if let Some(arg) = delimiter_command_arg(&t, self.src) {
                        self.finish(&seg, None);
                        self.custom_delim = arg != ";";
                    } else {
                        self.finish(&seg, Some(&t));
                    }
                    seg = Seg::new(false);
                }
                TokenKind::Semicolon
                    if !self.custom_delim
                        && seg.blocks == 0
                        && seg.mode != BlockMode::UntilBatchEnd =>
                {
                    pending_newlines = 0;
                    self.finish(&seg, Some(&t));
                    seg = Seg::new(false);
                }
                _ => {
                    pending_newlines = 0;
                    self.significant(&mut seg, i);
                }
            }
            i += 1;
        }
        self.finish(&seg, None);
    }

    fn blank_line(&mut self, seg: &mut Seg, i: usize) {
        if !seg.has_content() {
            seg.leading_comment = None;
            return;
        }
        let split = match self.opts.blank_line {
            BlankLineMode::Never => false,
            _ if seg.blocks > 0 || seg.is_routine() => false,
            BlankLineMode::Always => true,
            BlankLineMode::Smart => {
                if seg.paren > 0 {
                    false
                } else if let Some(first) = seg.first_sig {
                    let prev = self.tokens[seg.last_sig];
                    let next = self.next_sig(i).map(|j| self.tokens[j]);
                    !self.continues(&prev, next.as_ref(), self.complete_ending(seg.last_sig))
                        && !self.only_prefix(first, seg.last_sig)
                } else {
                    // Only an executable comment so far.
                    true
                }
            }
        };
        if split {
            self.finish(seg, None);
            *seg = Seg::new(true);
        }
    }

    /// The statement so far is only a prefix of one: a `WITH` list without
    /// its main statement, or `INSERT INTO target (columns)` without the rows.
    fn only_prefix(&self, first: usize, last: usize) -> bool {
        let lead = |w: &str| self.word(first, w);
        let main: &[&str] = if lead("WITH") {
            &[
                "SELECT", "INSERT", "UPDATE", "DELETE", "MERGE", "VALUES", "TABLE", "REPLACE",
            ]
        } else if lead("INSERT") || lead("REPLACE") {
            &[
                "VALUES",
                "VALUE",
                "SELECT",
                "SET",
                "DEFAULT",
                "WITH",
                "TABLE",
                "EXEC",
                "EXECUTE",
                "OUTPUT",
                "FORMAT",
                "ON",
                "RETURNING",
            ]
        } else {
            return false;
        };
        let mut depth = 0i32;
        for t in &self.tokens[first + 1..=last] {
            match t.kind {
                TokenKind::LParen => depth += 1,
                TokenKind::RParen => depth -= 1,
                _ if depth == 0 && word_in(t, self.src, main) => return false,
                _ => {}
            }
        }
        true
    }

    /// A last word that usually continues but ends this statement:
    /// `RESET ALL`, `DISCARD ALL`, `CLOSE ALL`, `INSERT … DEFAULT VALUES`.
    fn complete_ending(&self, last: usize) -> bool {
        let before = self.prev_sig(last);
        let before_is = |w: &str| before.is_some_and(|b| self.word(b, w));
        (self.word(last, "ALL")
            && [
                "RESET",
                "DISCARD",
                "CLOSE",
                "SHOW",
                "DEALLOCATE",
                "UNLISTEN",
                "PRIVILEGES",
            ]
            .iter()
            .any(|w| before_is(w)))
            || (self.word(last, "VALUES") && before_is("DEFAULT"))
    }

    fn continues(&self, prev: &Token, next: Option<&Token>, complete: bool) -> bool {
        if matches!(
            prev.kind,
            TokenKind::Comma | TokenKind::LParen | TokenKind::Operator | TokenKind::Dot
        ) {
            return true;
        }
        if !complete && word_in(prev, self.src, CONTINUE_AFTER) {
            return true;
        }
        match next {
            None => false,
            Some(n) => {
                matches!(
                    n.kind,
                    TokenKind::Comma | TokenKind::RParen | TokenKind::Operator | TokenKind::Dot
                ) || word_in(n, self.src, CONTINUE_BEFORE)
            }
        }
    }

    fn finish(&mut self, seg: &Seg, term: Option<&Token>) {
        let (first, last) = match (seg.first_sig, seg.exec_comment) {
            (Some(f), _) => (f, seg.last_sig),
            (None, Some((a, b))) => (a, b),
            (None, None) => return,
        };
        let start = self.tokens[first].start;
        let end = self.tokens[last].end;
        let outer_start = seg.leading_comment.map(|c| c.min(start)).unwrap_or(start);
        let outer_end = term.map(|t| t.end).unwrap_or(end).max(end);
        self.out.push(SplitStatement {
            span: StatementSpan {
                start,
                end,
                outer_start,
                outer_end,
                terminated: term.is_some(),
                is_client_command: false,
            },
            tokens: first..last + 1,
        });
    }

    fn significant(&mut self, seg: &mut Seg, i: usize) {
        let t = self.tokens[i];
        if seg.first_sig.is_none() {
            seg.first_sig = Some(i);
        }
        seg.last_sig = i;
        match t.kind {
            TokenKind::LParen => seg.paren += 1,
            TokenKind::RParen => seg.paren = (seg.paren - 1).max(0),
            _ => {}
        }
        self.header(seg, i);
        if seg.is_routine() {
            self.block_word(seg, i);
        }
    }

    /// Decide whether the statement is a routine whose body may contain `;`.
    fn header(&self, seg: &mut Seg, i: usize) {
        let t = self.tokens[i];
        match seg.header {
            Header::Done => {}
            Header::Start => {
                seg.header = Header::Done;
                seg.mode = BlockMode::Plain;
                if self.word(i, "CREATE") || self.word(i, "ALTER") {
                    seg.header = Header::AfterCreate { definer: false };
                    seg.mode = BlockMode::Undecided;
                } else if self.syntax.compound_blocks {
                    let next = self.next_sig(i);
                    if self.word(i, "BEGIN") && !self.is_tx_begin(next) {
                        // Anonymous block (T-SQL / PL/SQL style).
                        seg.mode = BlockMode::Compound;
                    } else if self.syntax.plsql_blocks && self.word(i, "DECLARE") {
                        // PL/SQL `DECLARE ... BEGIN ... END;`: the declarations
                        // end with `;` inside the block.
                        seg.mode = BlockMode::Compound;
                        seg.blocks = 1;
                        seg.declare_open = true;
                    } else if self.word(i, "IF") || self.word(i, "WHILE") {
                        seg.mode = BlockMode::Compound;
                    }
                }
            }
            Header::AfterCreate { definer } => {
                seg.header_len += 1;
                if word_in(&t, self.src, ROUTINE_WORDS)
                    || (self.syntax.plsql_blocks && word_in(&t, self.src, PLSQL_ROUTINE_WORDS))
                {
                    seg.header = Header::Done;
                    seg.mode = if !self.syntax.compound_blocks {
                        BlockMode::Atomic
                    } else if self.syntax.batch_separator.is_some() {
                        BlockMode::UntilBatchEnd
                    } else {
                        BlockMode::Compound
                    };
                    return;
                }
                let filler = word_in(&t, self.src, HEADER_FILLERS)
                    || (definer
                        && matches!(
                            t.kind,
                            TokenKind::Operator
                                | TokenKind::String
                                | TokenKind::QuotedIdentifier
                                | TokenKind::Identifier
                                | TokenKind::Variable
                                | TokenKind::LParen
                                | TokenKind::RParen
                                | TokenKind::Keyword
                        ));
                if !filler || seg.header_len > 16 {
                    seg.header = Header::Done;
                    seg.mode = BlockMode::Plain;
                } else if self.word(i, "DEFINER") {
                    seg.header = Header::AfterCreate { definer: true };
                }
            }
        }
    }

    /// `BEGIN` that starts a transaction rather than a block.
    fn is_tx_begin(&self, next: Option<usize>) -> bool {
        match next {
            None => true,
            Some(j) => {
                let t = &self.tokens[j];
                matches!(t.kind, TokenKind::Semicolon | TokenKind::Delimiter)
                    || word_in(t, self.src, TX_AFTER_BEGIN)
            }
        }
    }

    /// Look ahead from `i` for `target` before the end of the current
    /// statement part (`;`, `BEGIN`, `END`), skipping nested `CASE ... END`.
    fn lookahead(&self, i: usize, target: &str) -> bool {
        let mut depth = 0u32;
        let mut j = i;
        for _ in 0..2000 {
            let Some(k) = self.next_sig(j) else {
                return false;
            };
            j = k;
            let t = &self.tokens[k];
            if matches!(t.kind, TokenKind::Semicolon | TokenKind::Delimiter) {
                return false;
            }
            if self.word(k, "CASE") {
                depth += 1;
            } else if self.word(k, "END") {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
            } else if depth == 0 {
                if self.word(k, target) {
                    return true;
                }
                if self.word(k, "BEGIN") {
                    return false;
                }
            }
        }
        false
    }

    fn block_word(&self, seg: &mut Seg, i: usize) {
        let t = self.tokens[i];
        if !matches!(t.kind, TokenKind::Keyword | TokenKind::Identifier) {
            seg.skip_end_word = false;
            return;
        }
        if seg.skip_end_word {
            seg.skip_end_word = false;
            if word_in(&t, self.src, END_SUFFIXES) {
                return;
            }
        }
        let next = self.next_sig(i);
        let next_is = |w: &str| next.is_some_and(|j| self.word(j, w));
        let next_paren = next.is_some_and(|j| self.tokens[j].kind == TokenKind::LParen);
        if self.word(i, "END") {
            if seg.blocks > 0 {
                seg.blocks -= 1;
                seg.skip_end_word = true;
            }
            return;
        }
        if seg.mode == BlockMode::Atomic {
            if (self.word(i, "BEGIN") && next_is("ATOMIC"))
                || (seg.blocks > 0 && self.word(i, "CASE"))
            {
                seg.blocks += 1;
            }
            return;
        }
        if seg.declare_open && self.word(i, "BEGIN") {
            // The block was opened by its DECLARE.
            seg.declare_open = false;
            return;
        }
        let opens = if self.word(i, "BEGIN") {
            !self.is_tx_begin(next)
        } else if self.word(i, "CASE") || self.word(i, "LOOP") {
            true
        } else if self.word(i, "IF") {
            // `IF (a > 0) THEN`, `IF EXISTS (…) THEN`, `IF NOT done THEN` open a block;
            // `IF(a, b, c)` in an expression and `DROP … IF EXISTS` do not.
            // A bare condition (`IF done THEN`) opens one anywhere, as before.
            (self.statement_position(i) || (!next_paren && !next_is("EXISTS") && !next_is("NOT")))
                && self.lookahead(i, "THEN")
        } else if self.word(i, "WHILE") || self.word(i, "FOR") {
            self.lookahead(i, "DO")
        } else if self.word(i, "REPEAT") {
            !next_paren
        } else {
            false
        };
        if opens {
            seg.blocks += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    fn texts(src: &str, syn: &SqlSyntax) -> Vec<String> {
        split_statements(src, syn, &SplitOptions::default())
            .iter()
            .map(|s| s.text(src).to_string())
            .collect()
    }

    fn texts_with(src: &str, syn: &SqlSyntax, opts: &SplitOptions) -> Vec<String> {
        split_statements(src, syn, opts)
            .iter()
            .map(|s| s.text(src).to_string())
            .collect()
    }

    #[test]
    fn simple_split() {
        assert_eq!(
            texts("SELECT 1; SELECT 2;", &pg()),
            vec!["SELECT 1", "SELECT 2"]
        );
        assert_eq!(
            texts("SELECT 1;\n\n  SELECT 2  ", &pg()),
            vec!["SELECT 1", "SELECT 2"]
        );
        assert_eq!(
            texts("SELECT ';' FROM t; -- ;\n/* ; */ SELECT \"a;b\"", &pg()),
            vec!["SELECT ';' FROM t", "SELECT \"a;b\""]
        );
        assert!(texts("", &pg()).is_empty());
        assert!(texts(" ;; -- x\n ; ", &pg()).is_empty());
    }

    #[test]
    fn spans_and_comments() {
        let src = "-- head\nSELECT 1; -- trailing\nSELECT 2";
        let s = split_statements(src, &pg(), &SplitOptions::default());
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].outer_start, 0);
        assert_eq!(s[0].text(src), "SELECT 1");
        assert!(s[0].terminated);
        assert_eq!(&src[s[0].outer_start..s[0].outer_end], "-- head\nSELECT 1;");
        // The trailing comment belongs to the line of statement 1, not to statement 2
        assert_eq!(s[1].outer_start, s[1].start);
        assert!(!s[1].terminated);
        // Comments separated by a blank line are not attached
        let src = "-- note\n\nSELECT 1";
        let s = split_statements(src, &pg(), &SplitOptions::default());
        assert_eq!(s[0].outer_start, s[0].start);
    }

    #[test]
    fn dollar_quotes_and_strings_do_not_split() {
        let src = "DO $$ BEGIN PERFORM 1; END $$;\nCREATE FUNCTION f() RETURNS int AS $body$ SELECT 1; $body$ LANGUAGE sql;\nSELECT E'\\';'";
        assert_eq!(
            texts(src, &pg()),
            vec![
                "DO $$ BEGIN PERFORM 1; END $$",
                "CREATE FUNCTION f() RETURNS int AS $body$ SELECT 1; $body$ LANGUAGE sql",
                "SELECT E'\\';'"
            ]
        );
    }

    #[test]
    fn begin_atomic_bodies() {
        let src = "CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT CASE WHEN a > 0 THEN 1 END; SELECT 2; END;\nBEGIN;\nCOMMIT;";
        let t = texts(src, &pg());
        assert_eq!(t.len(), 3, "{t:?}");
        assert!(t[0].ends_with("SELECT 2; END"));
        assert_eq!(t[1], "BEGIN");
        assert_eq!(t[2], "COMMIT");
    }

    #[test]
    fn plsql_blocks_and_slash_lines() {
        let mut syn = SqlSyntax::ansi();
        syn.compound_blocks = true;
        syn.plsql_blocks = true;
        syn.batch_separator = Some("/");
        let src = "SELECT 1 FROM dual;\n\
            DECLARE\n  n NUMBER := 1;\nBEGIN\n  IF n > 0 THEN\n    n := n + 1;\n  END IF;\nEND;\n\
            BEGIN\n  NULL;\nEND;\n\
            CREATE OR REPLACE PROCEDURE p AS\nBEGIN\n  UPDATE t SET a = 1;\n  COMMIT;\nEND;\n/\n\
            CREATE OR REPLACE TYPE pt AS OBJECT (x NUMBER, y NUMBER);\n/\n\
            SELECT 2 FROM dual / 2;\n";
        let got = texts(src, &syn);
        assert_eq!(got.len(), 6, "{got:#?}");
        assert!(
            got[1].starts_with("DECLARE") && got[1].trim_end().ends_with("END"),
            "{}",
            got[1]
        );
        assert!(got[1].contains("n := n + 1;"));
        assert!(got[2].starts_with("BEGIN") && got[2].contains("NULL;"));
        assert!(
            got[3].starts_with("CREATE OR REPLACE PROCEDURE") && got[3].contains("COMMIT;"),
            "{}",
            got[3]
        );
        assert!(
            got[4].starts_with("CREATE OR REPLACE TYPE pt"),
            "{}",
            got[4]
        );
        assert!(got[5].starts_with("SELECT 2 FROM dual / 2"), "{}", got[5]);
        // T-SQL keeps DECLARE @x as a statement of its own.
        let tsql = texts("DECLARE @x int;\nSELECT @x;", &tsql());
        assert_eq!(tsql.len(), 2, "{tsql:#?}");
    }

    #[test]
    fn mysql_compound_blocks() {
        let src = "CREATE DEFINER=`root`@`localhost` PROCEDURE p(IN x INT)\nBEGIN\n  DECLARE i INT DEFAULT 0;\n  IF x > 0 THEN\n    SET i = 1;\n  ELSEIF x < 0 THEN SET i = 2;\n  END IF;\n  WHILE i < 10 DO SET i = i + 1; END WHILE;\n  REPEAT SET i = i - 1; UNTIL i = 0 END REPEAT;\n  lbl: LOOP LEAVE lbl; END LOOP lbl;\n  SELECT IF(i > 0, 'a', 'b'), REPEAT('x', 2);\n  CASE i WHEN 1 THEN SELECT 1; ELSE SELECT 2; END CASE;\nEND;\nSELECT 3;\nBEGIN;\nBEGIN WORK;\nSTART TRANSACTION;";
        let t = texts(src, &mysql());
        assert_eq!(t.len(), 5, "{t:#?}");
        assert!(
            t[0].starts_with("CREATE DEFINER") && t[0].ends_with("END CASE;\nEND"),
            "{}",
            t[0]
        );
        assert_eq!(
            &t[1..],
            &["SELECT 3", "BEGIN", "BEGIN WORK", "START TRANSACTION"]
        );
    }

    #[test]
    fn complete_endings_after_blank_line() {
        assert_eq!(
            texts("RESET ALL\n\nSELECT 1", &pg()),
            vec!["RESET ALL", "SELECT 1"]
        );
        assert_eq!(
            texts("INSERT INTO t DEFAULT VALUES\n\nSELECT 1", &pg()),
            vec!["INSERT INTO t DEFAULT VALUES", "SELECT 1"]
        );
        assert_eq!(
            texts("SELECT a FROM t UNION ALL\n\nSELECT b FROM u", &pg()).len(),
            1
        );
    }

    #[test]
    fn statement_prefixes_continue_after_blank_line() {
        let cte = "WITH old AS (SELECT id FROM items WHERE seen < 1)\n\nDELETE FROM items WHERE id IN (SELECT id FROM old)";
        assert_eq!(texts(cte, &pg()), vec![cte]);
        let two = "WITH a AS (SELECT 1), b AS (SELECT 2)\n\nSELECT * FROM a, b";
        assert_eq!(texts(two, &pg()), vec![two]);
        let insert = "INSERT INTO t (a, b)\n\nSELECT 1, 2";
        assert_eq!(texts(insert, &mysql()), vec![insert]);
        // Complete statements still split.
        assert_eq!(
            texts("INSERT INTO t (a) VALUES (1)\n\nSELECT 1", &pg()),
            vec!["INSERT INTO t (a) VALUES (1)", "SELECT 1"]
        );
        assert_eq!(
            texts("WITH a AS (SELECT 1) SELECT * FROM a\n\nSELECT 2", &pg()),
            vec!["WITH a AS (SELECT 1) SELECT * FROM a", "SELECT 2"]
        );
    }

    #[test]
    fn mysql_if_with_parenthesis_exists_not() {
        let src = "CREATE PROCEDURE p() BEGIN IF (1>0) THEN SELECT 1; END IF; IF EXISTS(SELECT 1) THEN SELECT 2; END IF; IF NOT done THEN SELECT 3; END IF; DROP TABLE IF EXISTS t; SELECT 4; END;\nSELECT 5;";
        let t = texts(src, &mysql());
        assert_eq!(t.len(), 2, "{t:#?}");
        assert!(t[0].ends_with("SELECT 4; END"), "{}", t[0]);
        assert_eq!(t[1], "SELECT 5");
        // A body that is one IF, without BEGIN.
        let src = "CREATE TRIGGER t3 BEFORE INSERT ON t FOR EACH ROW IF (NEW.a > 0) THEN SET NEW.b = 1; END IF;\nCREATE FUNCTION f(x INT) RETURNS INT DETERMINISTIC IF x > 0 THEN RETURN 1; END IF;\nSELECT 6;";
        let t = texts(src, &mysql());
        assert_eq!(t.len(), 3, "{t:#?}");
        assert!(t[0].ends_with("END IF"), "{}", t[0]);
        assert_eq!(t[2], "SELECT 6");
    }

    #[test]
    fn mysql_trigger_and_event() {
        let src = "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW BEGIN SET NEW.a = 1; SET NEW.b = 2; END;\nCREATE EVENT e ON SCHEDULE EVERY 1 HOUR DO BEGIN DELETE FROM log; END;\nCREATE TRIGGER t2 BEFORE UPDATE ON t FOR EACH ROW SET NEW.c = 3;\nCREATE TABLE repeat_log (repeat INT, `trigger` INT);\nSELECT 1;";
        let t = texts(src, &mysql());
        assert_eq!(t.len(), 5, "{t:#?}");
        assert!(t[0].ends_with("SET NEW.b = 2; END"));
        assert!(t[1].ends_with("DELETE FROM log; END"));
        assert_eq!(
            t[2],
            "CREATE TRIGGER t2 BEFORE UPDATE ON t FOR EACH ROW SET NEW.c = 3"
        );
        assert_eq!(t[4], "SELECT 1");
    }

    #[test]
    fn mysql_delimiter_scripts() {
        let src = "DELIMITER //\nCREATE PROCEDURE p()\nBEGIN\n  SELECT 1;\n  SELECT 2;\nEND//\nCALL p()//\nDELIMITER ;\nSELECT 3;";
        let t = texts(src, &mysql());
        assert_eq!(
            t,
            vec![
                "CREATE PROCEDURE p()\nBEGIN\n  SELECT 1;\n  SELECT 2;\nEND",
                "CALL p()",
                "SELECT 3"
            ]
        );
        let src =
            "delimiter $$\ncreate procedure p() begin select 1; end$$\ndelimiter ;\nselect 2;";
        assert_eq!(
            texts(src, &mysql()),
            vec!["create procedure p() begin select 1; end", "select 2"]
        );
        // Executable comments of mysqldump are statements
        let src = "/*!40101 SET NAMES utf8mb4 */;\n-- plain\nDROP TABLE IF EXISTS `t`;";
        assert_eq!(
            texts(src, &mysql()),
            vec!["/*!40101 SET NAMES utf8mb4 */", "DROP TABLE IF EXISTS `t`"]
        );
    }

    #[test]
    fn sqlite_triggers() {
        let src = "CREATE TRIGGER t AFTER INSERT ON a BEGIN INSERT INTO b VALUES (1); UPDATE c SET x = CASE WHEN 1 THEN 2 END; END;\nCREATE TEMP TRIGGER IF NOT EXISTS t2 AFTER DELETE ON a BEGIN DELETE FROM b; END;\nBEGIN TRANSACTION;\nBEGIN IMMEDIATE;\nSELECT 1;";
        let t = texts(src, &sqlite());
        assert_eq!(t.len(), 5, "{t:#?}");
        assert!(t[0].ends_with("END; END"));
        assert!(t[1].starts_with("CREATE TEMP TRIGGER"));
        assert_eq!(
            &t[2..],
            &["BEGIN TRANSACTION", "BEGIN IMMEDIATE", "SELECT 1"]
        );
    }

    #[test]
    fn tsql_batches() {
        let src = "SELECT 1;\nSELECT 'GO';\nGO\nCREATE PROCEDURE p AS\nBEGIN\n  SET NOCOUNT ON;\n  SELECT 2;\nEND\nGO\nIF @x = 1\nBEGIN\n  PRINT 'a';\n  PRINT 'b';\nEND\nGO 2\nBEGIN TRAN;\nCOMMIT;";
        let t = texts(src, &tsql());
        assert_eq!(t.len(), 6, "{t:#?}");
        assert_eq!(t[0], "SELECT 1");
        assert_eq!(t[1], "SELECT 'GO'");
        assert!(t[2].starts_with("CREATE PROCEDURE p AS") && t[2].ends_with("END"));
        assert!(t[3].starts_with("IF @x = 1") && t[3].ends_with("END"));
        assert_eq!(&t[4..], &["BEGIN TRAN", "COMMIT"]);
        // T-SQL procedure without BEGIN runs to the batch end
        let src = "CREATE PROC q AS SELECT 1; SELECT 2;\nGO\nSELECT 3";
        assert_eq!(
            texts(src, &tsql()),
            vec!["CREATE PROC q AS SELECT 1; SELECT 2;", "SELECT 3"]
        );
        let s = split_statements("SELECT 1\nGO", &tsql(), &SplitOptions::default());
        assert!(s[0].terminated);
    }

    #[test]
    fn blank_line_modes() {
        let src = "SELECT 1\n\nSELECT 2\nFROM t\n\nWHERE a = 1\n\nSELECT (\n\n1)";
        assert_eq!(
            texts(src, &pg()),
            vec![
                "SELECT 1",
                "SELECT 2\nFROM t\n\nWHERE a = 1",
                "SELECT (\n\n1)"
            ]
        );
        let never = SplitOptions {
            blank_line: BlankLineMode::Never,
            ..Default::default()
        };
        assert_eq!(texts_with(src, &pg(), &never).len(), 1);
        let always = SplitOptions {
            blank_line: BlankLineMode::Always,
            ..Default::default()
        };
        assert_eq!(texts_with(src, &pg(), &always).len(), 5);
        // Continuation after a trailing comma or keyword
        assert_eq!(texts("SELECT a,\n\nb FROM t", &pg()).len(), 1);
        assert_eq!(texts("SELECT a FROM\n\nt", &pg()).len(), 1);
        // Routine bodies are never split by blank lines
        assert_eq!(
            texts(
                "CREATE PROCEDURE p()\nBEGIN\n  SELECT 1;\n\n  SELECT 2;\nEND;",
                &mysql()
            )
            .len(),
            1
        );
    }

    #[test]
    fn client_commands() {
        let src = "@set x = 1\nSELECT ${x};\n@include other.sql\n\n@echo done";
        let s = split_statements(src, &pg(), &SplitOptions::default());
        let t: Vec<_> = s
            .iter()
            .map(|s| (s.text(src), s.is_client_command))
            .collect();
        assert_eq!(
            t,
            vec![
                ("@set x = 1", true),
                ("SELECT ${x}", false),
                ("@include other.sql", true),
                ("@echo done", true)
            ]
        );
        let off = SplitOptions {
            client_commands: false,
            ..Default::default()
        };
        let t = texts_with("@set x = 1\nSELECT 1;", &pg(), &off);
        assert_eq!(t, vec!["@set x = 1\nSELECT 1"]);
        // A command after an unterminated statement and a blank line
        let src = "SELECT 1\n\n@set y = 2\nSELECT 2";
        assert_eq!(
            texts(src, &pg()),
            vec!["SELECT 1", "@set y = 2", "SELECT 2"]
        );
    }

    #[test]
    fn custom_delimiter_option() {
        let opts = SplitOptions {
            delimiter: "/".into(),
            ..Default::default()
        };
        let src = "SELECT 1; SELECT 2\n/\nSELECT 3 /";
        assert_eq!(
            texts_with(src, &pg(), &opts),
            vec!["SELECT 1; SELECT 2", "SELECT 3"]
        );
    }

    #[test]
    fn statement_at_cursor() {
        let src = "SELECT 1;  -- c\n\nSELECT 2\n  FROM t;\n\n\n  -- lead\n  SELECT 3;";
        let opts = SplitOptions::default();
        let at = |o: usize| statement_at(src, o, &pg(), &opts).map(|s| s.text(src).to_string());
        assert_eq!(at(0).as_deref(), Some("SELECT 1"));
        assert_eq!(at(8).as_deref(), Some("SELECT 1")); // end of statement
        assert_eq!(at(9).as_deref(), Some("SELECT 1")); // after ;
        assert_eq!(at(14).as_deref(), Some("SELECT 1")); // in trailing comment
        assert_eq!(at(16).as_deref(), Some("SELECT 1")); // blank line right after
        let s2 = src.find("SELECT 2").unwrap();
        assert_eq!(at(s2 + 3).as_deref(), Some("SELECT 2\n  FROM t"));
        let t_end = src.find("FROM t;").unwrap() + 7;
        assert_eq!(at(t_end).as_deref(), Some("SELECT 2\n  FROM t"));
        assert_eq!(at(t_end + 1).as_deref(), Some("SELECT 2\n  FROM t")); // blank line below
        assert_eq!(at(t_end + 2), None); // two lines below
        let lead = src.find("-- lead").unwrap();
        assert_eq!(at(lead + 2).as_deref(), Some("SELECT 3"));
        assert_eq!(at(lead - 1).as_deref(), Some("SELECT 3")); // indentation before comment
        assert_eq!(at(src.len()).as_deref(), Some("SELECT 3"));
        assert_eq!(at(9999).as_deref(), Some("SELECT 3"));
        assert_eq!(statement_at("", 0, &pg(), &opts), None);
        // Indentation before a statement on the same line
        let src = "SELECT 1;\n    SELECT 2;";
        assert_eq!(
            statement_at(src, 12, &pg(), &opts).map(|s| s.start),
            Some(14)
        );
    }

    #[test]
    fn random_inputs_never_panic() {
        let mut rng = Rng::new(7);
        for round in 0..400 {
            let src = random_sql(&mut rng, 40);
            for syn in all_syntaxes() {
                let opts = SplitOptions {
                    blank_line: [
                        BlankLineMode::Never,
                        BlankLineMode::Smart,
                        BlankLineMode::Always,
                    ][round % 3],
                    delimiter: if round % 7 == 0 {
                        "//".into()
                    } else {
                        ";".into()
                    },
                    client_commands: round % 5 != 0,
                };
                let spans = split_statements(&src, &syn, &opts);
                let mut last = 0;
                for s in &spans {
                    assert!(
                        s.start <= s.end
                            && s.end <= src.len()
                            && s.outer_start <= s.start
                            && s.outer_end >= s.end
                    );
                    assert!(src.is_char_boundary(s.start) && src.is_char_boundary(s.end));
                    assert!(s.start >= last, "overlap in {src:?}");
                    last = s.end;
                }
                let off = floor_boundary(&src, rng.below(src.len() + 2));
                let _ = statement_at(&src, off, &syn, &opts);
            }
        }
    }
}
