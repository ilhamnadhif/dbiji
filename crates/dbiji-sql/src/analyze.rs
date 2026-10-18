//! Tolerant semantic analysis of one statement: query scopes, table
//! references with aliases, CTEs, derived tables and their output columns,
//! and qualified column references.
//!
//! The analyzer is a single pass over the significant tokens with a stack of
//! scopes (query blocks) and parentheses. It never fails: incomplete and
//! invalid SQL produce a best-effort model. It powers autocomplete,
//! diagnostics, hyperlinks ([`object_at`]) and the outline.

use crate::lexicon::{WordSet, ident_eq, reserved};
use crate::split::{SplitOptions, pick_statement, split_tokens};
use crate::token::{Token, TokenKind, is_word, tokenize, unquote_ident};
use dbiji_core::SqlSyntax;
use std::rc::Rc;

/// How a table reference is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefRole {
    /// Read in `FROM` / `JOIN` / `USING` / `MERGE ... USING` / `TABLE x`.
    Source,
    /// Written by `INSERT INTO` / `UPDATE` / `DELETE FROM` / `MERGE INTO`.
    Target,
    /// An existing object named by DDL or utility statements (`ALTER TABLE`,
    /// `DROP`, `TRUNCATE`, `REFERENCES`, `CREATE INDEX ... ON` ...).
    Ddl,
    /// The object created by `CREATE TABLE` / `CREATE VIEW`.
    Create,
}

/// A table (or CTE) referenced by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableReference {
    /// Database / catalog part of a three-part name.
    pub catalog: Option<String>,
    /// Schema qualifier, if written.
    pub schema: Option<String>,
    /// Unquoted name.
    pub name: String,
    /// Alias given in the statement.
    pub alias: Option<String>,
    /// How the statement uses the table.
    pub role: RefRole,
    /// The name refers to a CTE of the statement rather than a table.
    pub is_cte: bool,
    /// Byte range of the (qualified) name.
    pub start: usize,
    /// End of the name (exclusive).
    pub end: usize,
    /// Index of the query scope (see [`StatementAnalysis::subqueries`]; 0 is the statement).
    pub scope: usize,
}

/// A common table expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CteInfo {
    /// CTE name (unquoted).
    pub name: String,
    /// Explicit column list `WITH x(a, b)` or the names derived from its select list.
    pub columns: Vec<String>,
    /// Byte range of the CTE name.
    pub name_start: usize,
    /// End of the CTE name (exclusive).
    pub name_end: usize,
    /// Byte range of the body (inside the parentheses); empty when missing.
    pub start: usize,
    /// End of the body (exclusive).
    pub end: usize,
}

/// A subquery (derived table in `FROM`, or an expression subquery).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubqueryInfo {
    /// Alias of a derived table (`(SELECT ...) s`).
    pub alias: Option<String>,
    /// Output column names derived from the select list.
    pub columns: Vec<String>,
    /// Byte range inside the parentheses.
    pub start: usize,
    /// End inside the parentheses (exclusive).
    pub end: usize,
    /// Scope index of the subquery.
    pub scope: usize,
    /// It is a derived table (`FROM (...) alias`).
    pub derived: bool,
}

/// Result of [`analyze`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StatementAnalysis {
    /// Every table / CTE referenced by name, in text order.
    pub tables: Vec<TableReference>,
    /// Common table expressions.
    pub ctes: Vec<CteInfo>,
    /// Subqueries and derived tables.
    pub subqueries: Vec<SubqueryInfo>,
}

/// Object under a position, for cmd+click navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectReference {
    /// Schema of the table, if known.
    pub schema: Option<String>,
    /// Table (or view) name.
    pub table: String,
    /// Set when the position is on a column (`u.name`, `INSERT INTO t (name)`).
    pub column: Option<String>,
    /// Byte range of the identifier under the position.
    pub start: usize,
    /// End of the identifier (exclusive).
    pub end: usize,
}

// ---------------------------------------------------------------- model

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stmt {
    None,
    Select,
    Insert,
    Update,
    Delete,
    Merge,
    Ddl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Clause {
    Start,
    With,
    Select,
    Into,
    From,
    Join,
    On,
    Using,
    Where,
    GroupBy,
    Having,
    Window,
    OrderBy,
    Limit,
    Set,
    Values,
    Returning,
    Target,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScopeKind {
    Root,
    Subquery,
    Derived,
    Cte(usize),
    Branch,
}

#[derive(Debug, Clone)]
pub(crate) struct Scope {
    pub parent: Option<usize>,
    pub start: usize,
    pub end: usize,
    pub kind: ScopeKind,
    pub sources: Vec<usize>,
    pub ctes: Vec<usize>,
    pub items: Vec<SelectItem>,
    pub union_first: Option<usize>,
    pub stmt: Stmt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SourceKind {
    Table {
        catalog: Option<String>,
        schema: Option<String>,
        name: String,
    },
    Cte(usize),
    Derived(usize),
    Function(String),
    Unknown,
}

#[derive(Debug, Clone)]
pub(crate) struct Source {
    pub kind: SourceKind,
    pub alias: Option<String>,
    pub alias_columns: Vec<String>,
    pub scope: usize,
    pub role: RefRole,
    /// Byte range of the name (or of the parenthesised subquery).
    pub start: usize,
    pub end: usize,
    /// Name parts with their byte ranges.
    pub parts: Vec<(String, usize, usize)>,
}

impl Source {
    /// Name used to qualify columns: the alias, else the table/CTE name.
    pub fn qualifier(&self) -> Option<&str> {
        if let Some(a) = &self.alias {
            return Some(a);
        }
        match &self.kind {
            SourceKind::Table { name, .. } | SourceKind::Function(name) => Some(name),
            _ => self.parts.last().map(|p| p.0.as_str()),
        }
    }

    /// Visible to unqualified column references of its scope.
    pub fn in_column_scope(&self) -> bool {
        matches!(self.role, RefRole::Source | RefRole::Target)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Cte {
    pub name: String,
    pub columns: Vec<String>,
    pub body: Option<usize>,
    pub name_start: usize,
    pub name_end: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct SelectItem {
    pub name: Option<String>,
    /// `*` (None) or `q.*` (Some(q)).
    pub star: Option<Option<String>>,
}

#[derive(Debug, Clone)]
pub(crate) struct ColRef {
    /// Qualifier parts followed by the column name, with byte ranges.
    pub parts: Vec<(String, usize, usize)>,
    pub scope: usize,
    /// Followed by `(` (a function call, not a column).
    pub call: bool,
    /// Column of a known source without qualifier (INSERT column lists ...).
    pub implicit: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FromSt {
    None,
    Expect(RefRole),
    After(usize),
    AfterAs(usize),
    AfterAlias(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CteSt {
    None,
    ExpectName,
    AfterName(usize),
    ExpectBody(usize),
    AfterBody,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParenKind {
    Plain,
    JoinGroup,
    CteColumns(usize),
    AliasColumns(usize),
    /// Column names of a source: `INSERT INTO t (`, `CREATE INDEX ON t (`,
    /// `REFERENCES t (`, `ON CONFLICT (`, `COPY t (`.
    Columns(usize),
    /// `JOIN t USING (`
    UsingColumns,
}

/// Parser state at the cursor (for autocomplete).
#[derive(Debug, Clone, Copy)]
pub(crate) struct CursorCtx {
    pub scope: usize,
    pub stmt: Stmt,
    pub clause: Clause,
    pub from: FromSt,
    pub cte: CteSt,
    pub paren: Option<ParenKind>,
    pub started: bool,
    pub target: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Model {
    pub scopes: Vec<Scope>,
    pub sources: Vec<Source>,
    pub ctes: Vec<Cte>,
    pub cols: Vec<ColRef>,
    pub cursor: Option<CursorCtx>,
}

impl Model {
    /// CTEs visible from `scope` (innermost first).
    pub fn visible_ctes(&self, scope: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut cur = Some(scope);
        let mut guard = 0;
        while let Some(s) = cur {
            guard += 1;
            if guard > 64 {
                break;
            }
            let Some(sc) = self.scopes.get(s) else { break };
            out.extend(sc.ctes.iter().copied());
            if let Some(f) = sc.union_first
                && let Some(fs) = self.scopes.get(f)
            {
                out.extend(fs.ctes.iter().copied());
            }
            cur = sc.parent;
        }
        out
    }

    pub fn find_cte(&self, scope: usize, name: &str) -> Option<usize> {
        self.visible_ctes(scope)
            .into_iter()
            .find(|&c| ident_eq(&self.ctes[c].name, name))
    }

    /// Sources visible from `scope`: its own first, then those of enclosing
    /// scopes (correlated references). Each with its distance (0 = own).
    pub fn visible_sources(&self, scope: usize) -> Vec<(usize, u32)> {
        let mut out = Vec::new();
        let mut cur = Some(scope);
        let mut dist = 0;
        while let Some(s) = cur {
            let Some(sc) = self.scopes.get(s) else { break };
            out.extend(sc.sources.iter().map(|&i| (i, dist)));
            cur = sc.parent;
            dist += 1;
            if dist > 64 {
                break;
            }
        }
        out
    }

    /// Resolve a qualifier (alias or table name) to a source visible from `scope`.
    pub fn resolve_qualifier(&self, scope: usize, q: &str) -> Option<usize> {
        let vis = self.visible_sources(scope);
        // Aliases win over table names; nearer scopes win.
        vis.iter()
            .find(|(i, _)| {
                self.sources[*i]
                    .alias
                    .as_deref()
                    .is_some_and(|a| ident_eq(a, q))
            })
            .or_else(|| {
                vis.iter().find(|(i, _)| {
                    let s = &self.sources[*i];
                    s.alias.is_none() && s.parts.last().is_some_and(|p| ident_eq(&p.0, q))
                })
            })
            .map(|(i, _)| *i)
    }

    /// Output column names of a scope (first branch of set operations);
    /// `*` items are skipped.
    pub fn scope_columns(&self, scope: usize) -> Vec<String> {
        let scope = self
            .scopes
            .get(scope)
            .and_then(|s| s.union_first)
            .unwrap_or(scope);
        let Some(sc) = self.scopes.get(scope) else {
            return Vec::new();
        };
        sc.items
            .iter()
            .filter(|it| it.star.is_none())
            .filter_map(|it| it.name.clone())
            .collect()
    }
}

// ---------------------------------------------------------------- parser

#[derive(Debug, Clone)]
struct Ctx {
    id: usize,
    stmt: Stmt,
    clause: Clause,
    from: FromSt,
    cte: CteSt,
    item_start: Option<usize>,
    started: bool,
    target: Option<usize>,
    column_list_for: Option<usize>,
}

#[derive(Debug, Clone)]
enum Frame {
    Scope(Ctx),
    Paren(ParenKind),
}

/// Words that end a table reference / cannot be aliases.
const ALIAS_STOP: &[&str] = &[
    "WHERE",
    "JOIN",
    "ON",
    "USING",
    "LEFT",
    "RIGHT",
    "FULL",
    "INNER",
    "OUTER",
    "CROSS",
    "NATURAL",
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
    "QUALIFY",
    "SET",
    "VALUES",
    "RETURNING",
    "FOR",
    "INTO",
    "SELECT",
    "FROM",
    "WITH",
    "STRAIGHT_JOIN",
    "TABLESAMPLE",
    "PARTITION",
    "USE",
    "IGNORE",
    "FORCE",
    "LATERAL",
    "APPLY",
    "OUTPUT",
    "DEFAULT",
    "WHEN",
    "THEN",
    "PREWHERE",
    "FINAL",
    "SAMPLE",
    "GLOBAL",
    "SEMI",
    "ANTI",
    "ASOF",
    "LOCK",
    "AS",
    "DO",
    "SETTINGS",
    "FORMAT",
    "OPTION",
    "CONNECT",
    "START",
    "WINDOW",
    "MATCH_RECOGNIZE",
    "PIVOT",
    "UNPIVOT",
    "READ",
    "WRITE",
    "LOW_PRIORITY",
    "IN",
    "AND",
    "OR",
    "NOT",
];
const JOIN_MODIFIERS: &[&str] = &[
    "LEFT", "RIGHT", "FULL", "INNER", "CROSS", "NATURAL", "OUTER", "SEMI", "ANTI", "ASOF", "ANY",
];
const SELECT_MODIFIERS: &[&str] = &[
    "DISTINCT",
    "ALL",
    "DISTINCTROW",
    "SQL_CALC_FOUND_ROWS",
    "SQL_NO_CACHE",
    "SQL_CACHE",
    "HIGH_PRIORITY",
    "STRAIGHT_JOIN",
    "SQL_SMALL_RESULT",
    "SQL_BIG_RESULT",
    "SQL_BUFFER_RESULT",
];
const EXPLAIN_OPTS: &[&str] = &[
    "ANALYZE",
    "ANALYSE",
    "VERBOSE",
    "COSTS",
    "BUFFERS",
    "FORMAT",
    "QUERY",
    "PLAN",
    "EXTENDED",
    "PARTITIONS",
    "FOR",
    "JSON",
    "TEXT",
    "XML",
    "YAML",
    "TREE",
    "TRADITIONAL",
    "PIPELINE",
    "AST",
    "SYNTAX",
    "ESTIMATE",
];
const STATEMENT_WORDS: &[&str] = &[
    "SELECT", "WITH", "INSERT", "UPDATE", "DELETE", "MERGE", "VALUES", "TABLE", "REPLACE", "UPSERT",
];

/// Non-reserved keywords that are part of expressions, not column names.
const EXPR_KEYWORDS: &[&str] = &[
    "ILIKE",
    "NULLS",
    "FIRST",
    "LAST",
    "ROWS",
    "RANGE",
    "GROUPS",
    "PRECEDING",
    "FOLLOWING",
    "UNBOUNDED",
    "CURRENT",
    "ROW",
    "OVER",
    "PARTITION",
    "FILTER",
    "WITHIN",
    "INTERVAL",
    "ESCAPE",
    "ZONE",
    "AT",
    "MATCHED",
    "DO",
    "NOTHING",
    "CONFLICT",
    "KEY",
    "DUPLICATE",
    "IGNORE",
    "RECURSIVE",
    "MATERIALIZED",
    "CASCADE",
    "RESTRICT",
    "NO",
    "ACTION",
    "SEPARATOR",
    "EXCLUDE",
    "TIES",
    "OTHERS",
];

pub(crate) struct Analyzer<'a> {
    src: &'a str,
    t: Vec<Token>,
    cursor: Option<usize>,
    reserved: Rc<WordSet>,
    m: Model,
    stack: Vec<Frame>,
    end: usize,
    /// Fallback context; only used if the scope stack were ever empty.
    dummy: Ctx,
}

/// Analyze significant `tokens` of a statement in `src`. `cursor` records
/// the parser state there; tokens overlapping `skip` (the word being typed)
/// are ignored. `end` is the byte offset where unclosed scopes end.
pub(crate) fn build_model(
    src: &str,
    tokens: &[Token],
    cursor: Option<usize>,
    skip: Option<(usize, usize)>,
    end: usize,
) -> Model {
    let t: Vec<Token> = tokens
        .iter()
        .filter(|t| !t.is_trivia())
        .filter(|t| skip.is_none_or(|(s, e)| t.end <= s || t.start >= e))
        .copied()
        .collect();
    let start = t
        .first()
        .map(|t| t.start)
        .unwrap_or(0)
        .min(cursor.unwrap_or(usize::MAX));
    let mut a = Analyzer {
        src,
        t,
        cursor,
        reserved: reserved(),
        m: Model::default(),
        stack: Vec::new(),
        end,
        dummy: Ctx::new(0),
    };
    a.m.scopes.push(Scope {
        parent: None,
        start,
        end,
        kind: ScopeKind::Root,
        sources: Vec::new(),
        ctes: Vec::new(),
        items: Vec::new(),
        union_first: None,
        stmt: Stmt::None,
    });
    a.stack.push(Frame::Scope(Ctx::new(0)));
    a.run();
    a.m
}

impl Ctx {
    fn new(id: usize) -> Ctx {
        Ctx {
            id,
            stmt: Stmt::None,
            clause: Clause::Start,
            from: FromSt::None,
            cte: CteSt::None,
            item_start: None,
            started: false,
            target: None,
            column_list_for: None,
        }
    }
}

impl<'a> Analyzer<'a> {
    fn upper(&self, i: usize) -> String {
        match self.t.get(i) {
            Some(t) if matches!(t.kind, TokenKind::Keyword | TokenKind::Identifier) => {
                t.text(self.src).to_ascii_uppercase()
            }
            _ => String::new(),
        }
    }

    fn w(&self, i: usize, word: &str) -> bool {
        self.t.get(i).is_some_and(|t| is_word(t, self.src, word))
    }

    fn any(&self, i: usize, words: &[&str]) -> bool {
        words.iter().any(|w| self.w(i, w))
    }

    fn kind(&self, i: usize) -> Option<TokenKind> {
        self.t.get(i).map(|t| t.kind)
    }

    fn ident(&self, i: usize) -> Option<String> {
        let t = self.t.get(i)?;
        match t.kind {
            TokenKind::Identifier | TokenKind::Keyword => Some(t.text(self.src).to_string()),
            TokenKind::QuotedIdentifier => Some(unquote_ident(t.text(self.src)).into_owned()),
            _ => None,
        }
    }

    /// A word that can be a table / alias name (not a clause keyword).
    fn nameish(&self, i: usize) -> bool {
        match self.kind(i) {
            Some(TokenKind::QuotedIdentifier) => true,
            Some(TokenKind::Identifier) => !self.any(i, ALIAS_STOP),
            Some(TokenKind::Keyword) => {
                !self.any(i, ALIAS_STOP) && !self.reserved.contains(self.t[i].text(self.src))
            }
            _ => false,
        }
    }

    /// A word in a table position (not a clause keyword).
    fn refish(&self, i: usize) -> bool {
        self.kind(i).is_some_and(|k| k.is_word()) && !self.any(i, ALIAS_STOP)
    }

    fn ctx_mut(&mut self) -> &mut Ctx {
        for f in self.stack.iter_mut().rev() {
            if let Frame::Scope(c) = f {
                return c;
            }
        }
        // The root frame is never popped while tokens are processed.
        &mut self.dummy
    }

    fn ctx(&self) -> Option<&Ctx> {
        self.stack.iter().rev().find_map(|f| {
            if let Frame::Scope(c) = f {
                Some(c)
            } else {
                None
            }
        })
    }

    fn top_paren(&self) -> Option<ParenKind> {
        match self.stack.last() {
            Some(Frame::Paren(k)) => Some(*k),
            _ => None,
        }
    }

    fn set_clause(&mut self, c: Clause) {
        self.ctx_mut().clause = c;
    }

    fn snapshot(&mut self) {
        if self.m.cursor.is_some() {
            return;
        }
        let paren = self.top_paren();
        if let Some(c) = self.ctx() {
            self.m.cursor = Some(CursorCtx {
                scope: c.id,
                stmt: c.stmt,
                clause: c.clause,
                from: c.from,
                cte: c.cte,
                paren,
                started: c.started,
                target: c.target,
            });
        }
    }

    fn run(&mut self) {
        let n = self.t.len();
        let mut i = 0;
        let mut guard = 0usize;
        while i < n {
            if let Some(c) = self.cursor
                && self.t[i].start >= c
            {
                self.snapshot();
            }
            let next = self.step(i);
            i = if next > i { next } else { i + 1 };
            guard += 1;
            if guard > 4 * n + 16 {
                break;
            }
        }
        if self.cursor.is_some() {
            self.snapshot();
        }
        // Close everything still open at the end of the statement.
        while let Some(f) = self.stack.pop() {
            if let Frame::Scope(c) = f {
                self.close_item(&c, n);
                if let Some(s) = self.m.scopes.get_mut(c.id) {
                    s.end = s.end.max(self.end);
                    s.stmt = c.stmt;
                }
            }
        }
    }

    fn step(&mut self, i: usize) -> usize {
        match self.t[i].kind {
            TokenKind::LParen => {
                self.open_paren(i);
                i + 1
            }
            TokenKind::RParen => {
                self.close_paren(i);
                i + 1
            }
            _ => match self.top_paren() {
                None | Some(ParenKind::JoinGroup) => self.scope_token(i),
                Some(k) => self.paren_token(i, k),
            },
        }
    }

    fn new_scope(
        &mut self,
        kind: ScopeKind,
        start: usize,
        union_first: Option<usize>,
        parent: Option<usize>,
    ) -> usize {
        let id = self.m.scopes.len();
        self.m.scopes.push(Scope {
            parent,
            start,
            end: self.end,
            kind,
            sources: Vec::new(),
            ctes: Vec::new(),
            items: Vec::new(),
            union_first,
            stmt: Stmt::None,
        });
        id
    }

    fn open_paren(&mut self, i: usize) {
        let tok = self.t[i];
        let next_query = self.any(i + 1, &["SELECT", "WITH", "VALUES"]);
        let at_scope_level = matches!(self.top_paren(), None | Some(ParenKind::JoinGroup));
        let Some(c) = self.ctx().cloned() else { return };
        if at_scope_level && c.clause == Clause::Select && c.item_start.is_none() {
            self.ctx_mut().item_start = Some(i);
        }
        // Column list of a known table: INSERT INTO t (, REFERENCES t (, ...
        if let Some(s) = c.column_list_for {
            self.ctx_mut().column_list_for = None;
            if !next_query {
                self.stack.push(Frame::Paren(ParenKind::Columns(s)));
                return;
            }
        }
        if at_scope_level {
            if let CteSt::AfterName(id) = c.cte {
                self.stack.push(Frame::Paren(ParenKind::CteColumns(id)));
                return;
            }
            if let CteSt::ExpectBody(id) = c.cte {
                let scope = self.new_scope(ScopeKind::Cte(id), tok.end, None, Some(c.id));
                if let Some(ct) = self.m.ctes.get_mut(id) {
                    ct.body = Some(scope);
                }
                self.ctx_mut().cte = CteSt::AfterBody;
                self.stack.push(Frame::Scope(Ctx::new(scope)));
                return;
            }
            match c.from {
                FromSt::Expect(role) => {
                    if next_query {
                        let scope = self.new_scope(ScopeKind::Derived, tok.end, None, Some(c.id));
                        let src = self.push_source(
                            SourceKind::Derived(scope),
                            role,
                            tok.start,
                            tok.end,
                            Vec::new(),
                        );
                        self.ctx_mut().from = FromSt::After(src);
                        self.stack.push(Frame::Scope(Ctx::new(scope)));
                    } else {
                        self.stack.push(Frame::Paren(ParenKind::JoinGroup));
                    }
                    return;
                }
                FromSt::AfterAlias(s) if !next_query => {
                    self.stack.push(Frame::Paren(ParenKind::AliasColumns(s)));
                    return;
                }
                _ => {}
            }
            if c.clause == Clause::Using && c.stmt != Stmt::Delete && c.stmt != Stmt::Merge {
                self.stack.push(Frame::Paren(ParenKind::UsingColumns));
                return;
            }
        }
        if next_query {
            let scope = self.new_scope(ScopeKind::Subquery, tok.end, None, Some(c.id));
            self.stack.push(Frame::Scope(Ctx::new(scope)));
            return;
        }
        self.stack.push(Frame::Paren(ParenKind::Plain));
    }

    fn close_paren(&mut self, i: usize) {
        let tok = self.t[i];
        if self.stack.len() <= 1 {
            return; // unbalanced `)` at statement level
        }
        let Some(f) = self.stack.pop() else { return };
        match f {
            Frame::Scope(c) => {
                self.close_item(&c, i);
                if let Some(s) = self.m.scopes.get_mut(c.id) {
                    s.end = tok.start;
                    s.stmt = c.stmt;
                }
            }
            Frame::Paren(ParenKind::JoinGroup) => self.ctx_mut().from = FromSt::None,
            Frame::Paren(_) => {}
        }
    }

    fn push_source(
        &mut self,
        kind: SourceKind,
        role: RefRole,
        start: usize,
        end: usize,
        parts: Vec<(String, usize, usize)>,
    ) -> usize {
        let scope = self.ctx().map(|c| c.id).unwrap_or(0);
        let id = self.m.sources.len();
        self.m.sources.push(Source {
            kind,
            alias: None,
            alias_columns: Vec::new(),
            scope,
            role,
            start,
            end,
            parts,
        });
        if let Some(s) = self.m.scopes.get_mut(scope) {
            s.sources.push(id);
        }
        id
    }

    /// Dotted name at `i`: parts and the index after it. Stops before a
    /// word that starts at/after the cursor or is a clause keyword not glued
    /// to the dot.
    fn dotted(&self, i: usize) -> (Vec<(String, usize, usize)>, usize, bool) {
        let mut parts = Vec::new();
        let Some(first) = self.ident(i) else {
            return (parts, i, false);
        };
        parts.push((first, self.t[i].start, self.t[i].end));
        let mut j = i + 1;
        let mut trailing_dot = false;
        while self.kind(j) == Some(TokenKind::Dot) && parts.len() < 4 {
            let dot = self.t[j];
            let Some(next) = self.t.get(j + 1) else {
                trailing_dot = true;
                j += 1;
                break;
            };
            let glued = next.start == dot.end;
            // The cursor sits right after the dot: the next word is not part of the name.
            let cursor_between = self
                .cursor
                .is_some_and(|c| dot.end <= c && c <= next.start && c < next.end);
            let ok =
                next.kind.is_word() && !cursor_between && (glued || !self.any(j + 1, ALIAS_STOP));
            if !ok {
                trailing_dot = true;
                j += 1;
                break;
            }
            if let Some(p) = self.ident(j + 1) {
                parts.push((p, next.start, next.end));
            }
            j += 2;
        }
        (parts, j, trailing_dot)
    }

    fn parse_ref(&mut self, i: usize, role: RefRole) -> usize {
        let mut j = i;
        while self.any(j, &["LATERAL", "ONLY", "LOW_PRIORITY", "IGNORE"])
            && self
                .kind(j + 1)
                .is_some_and(|k| k.is_word() || k == TokenKind::LParen)
        {
            j += 1;
        }
        match self.kind(j) {
            Some(TokenKind::LParen) => return j, // derived table / join group
            Some(TokenKind::Parameter | TokenKind::Variable) => {
                let t = self.t[j];
                let s = self.push_source(SourceKind::Unknown, role, t.start, t.end, Vec::new());
                self.ctx_mut().from = FromSt::After(s);
                return j + 1;
            }
            Some(k) if k.is_word() => {}
            _ => return j,
        }
        let (parts, mut next, trailing_dot) = self.dotted(j);
        if parts.is_empty() {
            return j + 1;
        }
        let start = parts[0].1;
        let end = parts.last().map(|p| p.2).unwrap_or(start);
        let scope = self.ctx().map(|c| c.id).unwrap_or(0);
        let kind = if role == RefRole::Source
            && !trailing_dot
            && self.kind(next) == Some(TokenKind::LParen)
        {
            // Table function: skip its arguments. (For targets and DDL the
            // parenthesis is a column list.)
            let mut depth = 0i32;
            let mut k = next;
            while k < self.t.len() {
                match self.t[k].kind {
                    TokenKind::LParen => depth += 1,
                    TokenKind::RParen => {
                        depth -= 1;
                        if depth <= 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                k += 1;
            }
            next = k + 1;
            SourceKind::Function(parts.last().map(|p| p.0.clone()).unwrap_or_default())
        } else if parts.len() == 1 && self.m.find_cte(scope, &parts[0].0).is_some() {
            SourceKind::Cte(self.m.find_cte(scope, &parts[0].0).unwrap_or(0))
        } else {
            let mut names: Vec<String> = parts.iter().map(|p| p.0.clone()).collect();
            let name = names.pop().unwrap_or_default();
            let schema = names.pop();
            let catalog = names.pop();
            SourceKind::Table {
                catalog,
                schema,
                name,
            }
        };
        let s = self.push_source(kind, role, start, end, parts);
        let ctx = self.ctx_mut();
        ctx.from = FromSt::After(s);
        if role == RefRole::Target && ctx.target.is_none() {
            ctx.target = Some(s);
            if ctx.stmt == Stmt::Insert {
                ctx.column_list_for = Some(s);
            }
        }
        // PostgreSQL `ONLY t *`
        if self.kind(next) == Some(TokenKind::Operator) && self.t[next].text(self.src) == "*" {
            next += 1;
        }
        next
    }

    fn close_item(&mut self, c: &Ctx, end_idx: usize) {
        if c.clause != Clause::Select {
            return;
        }
        let Some(s) = c.item_start else { return };
        if s >= end_idx || end_idx > self.t.len() {
            return;
        }
        let item = self.item_output(s, end_idx);
        if let Some(sc) = self.m.scopes.get_mut(c.id) {
            sc.items.push(item);
        }
    }

    fn item_output(&self, s: usize, e: usize) -> SelectItem {
        let toks = &self.t[s..e];
        let is_star = |t: &Token| t.kind == TokenKind::Operator && t.text(self.src) == "*";
        let mut item = SelectItem {
            name: None,
            star: None,
        };
        let n = toks.len();
        if n == 1 && is_star(&toks[0]) {
            item.star = Some(None);
            return item;
        }
        if n >= 3 && is_star(&toks[n - 1]) && toks[n - 2].kind == TokenKind::Dot {
            item.star = Some(self.ident(e - 3));
            return item;
        }
        let last = &toks[n - 1];
        let last_name = self.ident(e - 1);
        if n >= 2 && last.kind.is_word() {
            let prev = &toks[n - 2];
            if is_word(prev, self.src, "AS") {
                item.name = last_name;
                return item;
            }
            let prev_reserved =
                prev.kind == TokenKind::Keyword && self.reserved.contains(prev.text(self.src));
            let implicit = !matches!(
                prev.kind,
                TokenKind::Dot | TokenKind::Operator | TokenKind::LParen | TokenKind::Comma
            ) && !prev_reserved
                && (last.kind != TokenKind::Keyword
                    || !self.reserved.contains(last.text(self.src)));
            if implicit {
                item.name = last_name;
                return item;
            }
        }
        // Plain column reference `a` / `t.a`.
        if toks
            .iter()
            .all(|t| t.kind.is_word() || t.kind == TokenKind::Dot)
            && last.kind.is_word()
        {
            item.name = last_name;
            return item;
        }
        // Single function call `f(...)` is named after the function.
        if n >= 3
            && toks[0].kind.is_word()
            && toks[1].kind == TokenKind::LParen
            && last.kind == TokenKind::RParen
        {
            item.name = self.ident(s).map(|f| f.to_lowercase());
        }
        item
    }

    fn begin_select(&mut self, i: usize) -> usize {
        self.set_clause(Clause::Select);
        let ctx = self.ctx_mut();
        ctx.item_start = None;
        ctx.from = FromSt::None;
        if ctx.stmt == Stmt::None {
            ctx.stmt = Stmt::Select;
        }
        let mut j = i + 1;
        loop {
            if self.w(j, "DISTINCT")
                && self.w(j + 1, "ON")
                && self.kind(j + 2) == Some(TokenKind::LParen)
            {
                // DISTINCT ON (...)
                let mut depth = 0;
                let mut k = j + 2;
                while k < self.t.len() {
                    match self.t[k].kind {
                        TokenKind::LParen => depth += 1,
                        TokenKind::RParen => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    k += 1;
                }
                j = k + 1;
            } else if self.any(j, SELECT_MODIFIERS) {
                j += 1;
            } else if self.w(j, "TOP") {
                j += 1;
                if self.kind(j) == Some(TokenKind::LParen) {
                    while j < self.t.len() && self.kind(j) != Some(TokenKind::RParen) {
                        j += 1;
                    }
                }
                j += 1;
                if self.w(j, "PERCENT") {
                    j += 1;
                }
            } else {
                break;
            }
        }
        j.min(self.t.len())
    }

    fn new_branch(&mut self, i: usize) -> usize {
        let Some(c) = self.ctx().cloned() else {
            return i + 1;
        };
        self.close_item(&c, i);
        let tok = self.t[i];
        let mut j = i + 1;
        if self.any(j, &["ALL", "DISTINCT"]) {
            j += 1;
        }
        let (parent, kind, first) = match self.m.scopes.get_mut(c.id) {
            Some(s) => {
                s.end = tok.start;
                s.stmt = c.stmt;
                (s.parent, s.kind, s.union_first.unwrap_or(c.id))
            }
            None => (None, ScopeKind::Root, c.id),
        };
        let start = self
            .t
            .get(j.saturating_sub(1))
            .map(|t| t.end)
            .unwrap_or(tok.end);
        let kind = if kind == ScopeKind::Root {
            ScopeKind::Root
        } else {
            ScopeKind::Branch
        };
        let id = self.new_scope(kind, start, Some(first), parent);
        let ctx = self.ctx_mut();
        *ctx = Ctx::new(id);
        ctx.stmt = c.stmt;
        j
    }

    fn new_statement(&mut self, i: usize) -> usize {
        let Some(c) = self.ctx().cloned() else {
            return i + 1;
        };
        self.close_item(&c, i);
        let tok = self.t[i];
        let parent = match self.m.scopes.get_mut(c.id) {
            Some(s) => {
                s.end = tok.start;
                s.stmt = c.stmt;
                s.parent
            }
            None => None,
        };
        let id = self.new_scope(ScopeKind::Root, tok.end, None, parent);
        // CTEs of the enclosing statement stay visible to the inner statements.
        let ctx = self.ctx_mut();
        *ctx = Ctx::new(id);
        i + 1
    }

    fn comma(&mut self, i: usize) {
        let Some(c) = self.ctx().cloned() else { return };
        match c.clause {
            Clause::Select => {
                self.close_item(&c, i);
                self.ctx_mut().item_start = None;
            }
            Clause::From => self.ctx_mut().from = FromSt::Expect(RefRole::Source),
            // `FROM a JOIN b ON cond, c`: the FROM list continues.
            Clause::Join | Clause::On | Clause::Using => {
                self.set_clause(Clause::From);
                self.ctx_mut().from = FromSt::Expect(RefRole::Source);
            }
            Clause::Target => {
                let role = match c.from {
                    FromSt::After(s) | FromSt::AfterAs(s) | FromSt::AfterAlias(s) => self
                        .m
                        .sources
                        .get(s)
                        .map(|x| x.role)
                        .unwrap_or(RefRole::Target),
                    _ if c.stmt == Stmt::Ddl => RefRole::Ddl,
                    _ => RefRole::Target,
                };
                self.ctx_mut().from = FromSt::Expect(role);
            }
            Clause::With if c.cte == CteSt::AfterBody => {
                self.ctx_mut().cte = CteSt::ExpectName;
            }
            _ => {}
        }
    }

    /// Token inside parentheses that are not a query scope.
    fn paren_token(&mut self, i: usize, kind: ParenKind) -> usize {
        let t = self.t[i];
        match kind {
            ParenKind::CteColumns(c) => {
                if let Some(name) = self.ident(i)
                    && let Some(ct) = self.m.ctes.get_mut(c)
                {
                    ct.columns.push(name);
                }
                i + 1
            }
            ParenKind::AliasColumns(s) => {
                if let Some(name) = self.ident(i)
                    && let Some(src) = self.m.sources.get_mut(s)
                {
                    src.alias_columns.push(name);
                }
                i + 1
            }
            ParenKind::Columns(s) => {
                if t.kind.is_word() {
                    let scope = self.ctx().map(|c| c.id).unwrap_or(0);
                    if let Some(name) = self.ident(i) {
                        self.m.cols.push(ColRef {
                            parts: vec![(name, t.start, t.end)],
                            scope,
                            call: false,
                            implicit: Some(s),
                        });
                    }
                }
                i + 1
            }
            ParenKind::UsingColumns => i + 1,
            ParenKind::Plain | ParenKind::JoinGroup => {
                if self.w(i, "REFERENCES") {
                    return self.references(i);
                }
                if t.kind.is_word() {
                    self.expr_word(i)
                } else {
                    i + 1
                }
            }
        }
    }

    fn references(&mut self, i: usize) -> usize {
        let next = self.ref_or_expect(i + 1, RefRole::Ddl);
        if let Some(c) = self.ctx()
            && let FromSt::After(s) = c.from
        {
            let ctx = self.ctx_mut();
            ctx.column_list_for = Some(s);
            ctx.from = FromSt::None;
        }
        next
    }

    fn expr_word(&mut self, i: usize) -> usize {
        let t = self.t[i];
        let scope = self.ctx().map(|c| c.id).unwrap_or(0);
        if self.kind(i + 1) == Some(TokenKind::Dot) {
            let (parts, next, _) = self.dotted(i);
            let call = self.kind(next) == Some(TokenKind::LParen);
            if !parts.is_empty() {
                self.m.cols.push(ColRef {
                    parts,
                    scope,
                    call,
                    implicit: None,
                });
            }
            return next.max(i + 1);
        }
        let call = self.kind(i + 1) == Some(TokenKind::LParen);
        let keyword_like = t.kind == TokenKind::Keyword
            && (self.reserved.contains(t.text(self.src))
                || self.any(i, EXPR_KEYWORDS)
                || self.any(i, ALIAS_STOP));
        if !call
            && !keyword_like
            && let Some(name) = self.ident(i)
        {
            self.m.cols.push(ColRef {
                parts: vec![(name, t.start, t.end)],
                scope,
                call,
                implicit: None,
            });
        }
        i + 1
    }

    /// Statement keyword at the start of a scope.
    fn dispatch(&mut self, i: usize, word: &str) -> Option<usize> {
        match word {
            "WITH" => {
                self.set_clause(Clause::With);
                self.ctx_mut().cte = CteSt::ExpectName;
                Some(if self.w(i + 1, "RECURSIVE") {
                    i + 2
                } else {
                    i + 1
                })
            }
            "SELECT" => Some(self.begin_select(i)),
            "VALUES" => {
                self.set_clause(Clause::Values);
                Some(i + 1)
            }
            "TABLE" => {
                self.set_clause(Clause::From);
                let ctx = self.ctx_mut();
                ctx.stmt = Stmt::Select;
                ctx.from = FromSt::Expect(RefRole::Source);
                Some(i + 1)
            }
            "INSERT" | "REPLACE" | "UPSERT" => {
                self.ctx_mut().stmt = Stmt::Insert;
                self.set_clause(Clause::Start);
                Some(i + 1)
            }
            "UPDATE" => {
                self.ctx_mut().stmt = Stmt::Update;
                self.set_clause(Clause::Target);
                self.ctx_mut().from = FromSt::Expect(RefRole::Target);
                let mut j = i + 1;
                if self.w(j, "OR") {
                    j += 2; // SQLite UPDATE OR REPLACE
                }
                Some(j)
            }
            "DELETE" => {
                self.ctx_mut().stmt = Stmt::Delete;
                Some(i + 1)
            }
            "MERGE" => {
                self.ctx_mut().stmt = Stmt::Merge;
                Some(i + 1)
            }
            "EXPLAIN" => {
                let mut j = i + 1;
                for _ in 0..16 {
                    if self.kind(j) == Some(TokenKind::LParen) {
                        while j < self.t.len() && self.kind(j) != Some(TokenKind::RParen) {
                            j += 1;
                        }
                        j += 1;
                    } else if self.any(j, EXPLAIN_OPTS) || self.kind(j) == Some(TokenKind::Operator)
                    {
                        j += 1;
                    } else {
                        break;
                    }
                }
                if self.kind(j).is_some_and(|k| k.is_word()) && !self.any(j, STATEMENT_WORDS) {
                    self.ctx_mut().stmt = Stmt::Ddl;
                    return Some(self.parse_ref(j, RefRole::Ddl));
                }
                self.ctx_mut().started = false;
                Some(j)
            }
            "DESCRIBE" | "DESC" => {
                if self.any(i + 1, STATEMENT_WORDS) {
                    self.ctx_mut().started = false;
                    return Some(i + 1);
                }
                self.ctx_mut().stmt = Stmt::Ddl;
                Some(self.ref_or_expect(i + 1, RefRole::Ddl))
            }
            "CREATE" => Some(self.create(i)),
            "ALTER" => {
                self.ctx_mut().stmt = Stmt::Ddl;
                if self.w(i + 1, "TABLE") {
                    let mut j = i + 2;
                    j = self.skip_if_exists(j);
                    if self.w(j, "ONLY") {
                        j += 1;
                    }
                    let next = self.ref_or_expect(j, RefRole::Ddl);
                    let ctx = self.ctx_mut();
                    if let FromSt::After(s) = ctx.from {
                        ctx.target = Some(s);
                        ctx.from = FromSt::None;
                    }
                    return Some(next);
                }
                Some(i + 1)
            }
            "DROP" | "TRUNCATE" => {
                self.ctx_mut().stmt = Stmt::Ddl;
                let mut j = i + 1;
                if word == "DROP" {
                    if self.w(j, "TEMPORARY") {
                        j += 1;
                    }
                    if self.w(j, "MATERIALIZED") || self.w(j, "FOREIGN") {
                        j += 1;
                    }
                    if !self.any(j, &["TABLE", "VIEW"]) {
                        return Some(i + 1);
                    }
                    j += 1;
                } else if self.w(j, "TABLE") {
                    j += 1;
                }
                j = self.skip_if_exists(j);
                self.set_clause(Clause::Target);
                self.ctx_mut().from = FromSt::Expect(RefRole::Ddl);
                Some(j)
            }
            "LOCK" => {
                self.ctx_mut().stmt = Stmt::Ddl;
                let mut j = i + 1;
                if self.any(j, &["TABLE", "TABLES"]) {
                    j += 1;
                }
                self.set_clause(Clause::Target);
                self.ctx_mut().from = FromSt::Expect(RefRole::Ddl);
                Some(j)
            }
            "GRANT" | "REVOKE" => {
                self.ctx_mut().stmt = Stmt::Ddl;
                let on = (i + 1..self.t.len()).find(|&j| self.w(j, "ON"))?;
                let mut j = on + 1;
                if self.w(j, "TABLE") {
                    j += 1;
                }
                if self.any(
                    j,
                    &[
                        "ALL",
                        "SCHEMA",
                        "DATABASE",
                        "FUNCTION",
                        "PROCEDURE",
                        "SEQUENCE",
                    ],
                ) {
                    return Some(self.t.len());
                }
                self.set_clause(Clause::Target);
                self.ctx_mut().from = FromSt::Expect(RefRole::Ddl);
                Some(j)
            }
            "VACUUM" | "ANALYZE" | "ANALYSE" | "OPTIMIZE" | "REINDEX" | "CLUSTER" | "CHECK"
            | "REPAIR" | "CHECKSUM" => {
                self.ctx_mut().stmt = Stmt::Ddl;
                let mut j = i + 1;
                for _ in 0..8 {
                    if self.kind(j) == Some(TokenKind::LParen) {
                        while j < self.t.len() && self.kind(j) != Some(TokenKind::RParen) {
                            j += 1;
                        }
                        j += 1;
                    } else if self.any(
                        j,
                        &[
                            "TABLE",
                            "FULL",
                            "FREEZE",
                            "VERBOSE",
                            "ANALYZE",
                            "LOCAL",
                            "NO_WRITE_TO_BINLOG",
                            "CONCURRENTLY",
                            "INDEX",
                        ],
                    ) {
                        j += 1;
                    } else {
                        break;
                    }
                }
                if self.kind(j).is_some_and(|k| k.is_word()) {
                    self.set_clause(Clause::Target);
                    self.ctx_mut().from = FromSt::Expect(RefRole::Ddl);
                }
                Some(j)
            }
            "SHOW" => {
                self.ctx_mut().stmt = Stmt::Ddl;
                let from = (i + 1..(i + 5).min(self.t.len())).find(|&j| {
                    self.any(j, &["FROM", "IN"]) || (self.w(j, "TABLE") && self.w(j - 1, "CREATE"))
                });
                match from {
                    Some(j) => Some(self.ref_or_expect(j + 1, RefRole::Ddl)),
                    None => Some(self.t.len()),
                }
            }
            "COPY" => {
                if self.kind(i + 1).is_some_and(|k| k.is_word()) {
                    self.ctx_mut().stmt = Stmt::Ddl;
                    let next = self.parse_ref(i + 1, RefRole::Source);
                    let ctx = self.ctx_mut();
                    if let FromSt::After(s) = ctx.from {
                        ctx.column_list_for = Some(s);
                    }
                    ctx.from = FromSt::None;
                    return Some(next);
                }
                Some(i + 1)
            }
            "COMMENT" => {
                self.ctx_mut().stmt = Stmt::Ddl;
                if self.w(i + 1, "ON") && self.any(i + 2, &["TABLE", "VIEW", "COLUMN"]) {
                    let column = self.w(i + 2, "COLUMN");
                    let (mut parts, next, _) = self.dotted(i + 3);
                    if column && parts.len() >= 2 {
                        let col = parts.pop();
                        let scope = self.ctx().map(|c| c.id).unwrap_or(0);
                        let start = parts[0].1;
                        let end = parts.last().map(|p| p.2).unwrap_or(start);
                        let mut names: Vec<String> = parts.iter().map(|p| p.0.clone()).collect();
                        let name = names.pop().unwrap_or_default();
                        let schema = names.pop();
                        let s = self.push_source(
                            SourceKind::Table {
                                catalog: None,
                                schema,
                                name,
                            },
                            RefRole::Ddl,
                            start,
                            end,
                            parts,
                        );
                        if let Some(c) = col {
                            self.m.cols.push(ColRef {
                                parts: vec![c],
                                scope,
                                call: false,
                                implicit: Some(s),
                            });
                        }
                        return Some(next);
                    }
                    return Some(self.ref_or_expect(i + 3, RefRole::Ddl));
                }
                Some(self.t.len())
            }
            _ => None,
        }
    }

    /// Parse a table name at `j`, or — when it is still missing (the cursor
    /// is there) — expect one.
    fn ref_or_expect(&mut self, j: usize, role: RefRole) -> usize {
        if self.refish(j) {
            return self.parse_ref(j, role);
        }
        if self.kind(j).is_none() {
            self.set_clause(Clause::Target);
            self.ctx_mut().from = FromSt::Expect(role);
        }
        j
    }

    fn skip_if_exists(&self, mut j: usize) -> usize {
        if self.w(j, "IF") {
            j += 1;
            if self.w(j, "NOT") {
                j += 1;
            }
            if self.w(j, "EXISTS") {
                j += 1;
            }
        }
        j
    }

    fn create(&mut self, i: usize) -> usize {
        self.ctx_mut().stmt = Stmt::Ddl;
        let n = self.t.len();
        let kind = (i + 1..(i + 10).min(n)).find(|&j| {
            self.any(
                j,
                &[
                    "TABLE",
                    "VIEW",
                    "INDEX",
                    "TRIGGER",
                    "PROCEDURE",
                    "FUNCTION",
                    "EVENT",
                    "PROC",
                ],
            )
        });
        let Some(k) = kind else { return i + 1 };
        match self.upper(k).as_str() {
            "TABLE" | "VIEW" => {
                let j = self.skip_if_exists(k + 1);
                let (parts, mut next, _) = self.dotted(j);
                if !parts.is_empty() {
                    let start = parts[0].1;
                    let end = parts.last().map(|p| p.2).unwrap_or(start);
                    let mut names: Vec<String> = parts.iter().map(|p| p.0.clone()).collect();
                    let name = names.pop().unwrap_or_default();
                    let schema = names.pop();
                    let catalog = names.pop();
                    self.push_source(
                        SourceKind::Table {
                            catalog,
                            schema,
                            name,
                        },
                        RefRole::Create,
                        start,
                        end,
                        parts,
                    );
                }
                // A view's column list, then `AS query`.
                if self.upper(k) == "VIEW" {
                    if self.kind(next) == Some(TokenKind::LParen) {
                        while next < n && self.kind(next) != Some(TokenKind::RParen) {
                            next += 1;
                        }
                        next += 1;
                    }
                    while next < n && !self.w(next, "AS") {
                        next += 1;
                    }
                    self.ctx_mut().started = false;
                    return (next + 1).min(n.max(next));
                }
                // CREATE TABLE ... AS SELECT
                if self.w(next, "AS") {
                    self.ctx_mut().started = false;
                    return next + 1;
                }
                next
            }
            "INDEX" => {
                let Some(on) = (k + 1..(k + 6).min(n)).find(|&j| self.w(j, "ON")) else {
                    return k + 1;
                };
                let mut j = on + 1;
                if self.w(j, "ONLY") {
                    j += 1;
                }
                let next = self.ref_or_expect(j, RefRole::Ddl);
                let ctx = self.ctx_mut();
                if let FromSt::After(s) = ctx.from {
                    ctx.column_list_for = Some(s);
                    ctx.target = Some(s);
                    ctx.from = FromSt::None;
                }
                let mut next = next;
                if self.w(next, "USING") && self.kind(next + 1).is_some_and(|k| k.is_word()) {
                    next += 2;
                }
                next
            }
            "TRIGGER" => {
                let on = (k + 1..(k + 12).min(n)).find(|&j| self.w(j, "ON"));
                match on {
                    Some(on) => {
                        let next = self.ref_or_expect(on + 1, RefRole::Ddl);
                        if let FromSt::After(_) = self.ctx_mut().from {
                            self.ctx_mut().from = FromSt::None;
                        }
                        next
                    }
                    None => k + 1,
                }
            }
            _ => k + 1,
        }
    }

    fn clause_word(&mut self, i: usize, word: &str) -> Option<usize> {
        let c = self.ctx()?.clone();
        let next_paren = self.kind(i + 1) == Some(TokenKind::LParen);
        match word {
            "SELECT" => Some(self.begin_select(i)),
            "FROM" => {
                self.close_item(&c, i);
                if c.stmt == Stmt::Delete && c.target.is_none() {
                    self.set_clause(Clause::Target);
                    self.ctx_mut().from = FromSt::Expect(RefRole::Target);
                } else {
                    self.set_clause(Clause::From);
                    self.ctx_mut().from = FromSt::Expect(RefRole::Source);
                }
                Some(i + 1)
            }
            "JOIN" | "STRAIGHT_JOIN" | "APPLY" => {
                if matches!(
                    c.clause,
                    Clause::From | Clause::Join | Clause::On | Clause::Using | Clause::Target
                ) {
                    self.set_clause(Clause::Join);
                    self.ctx_mut().from = FromSt::Expect(RefRole::Source);
                    Some(i + 1)
                } else {
                    None
                }
            }
            w if JOIN_MODIFIERS.contains(&w) && !next_paren => {
                if matches!(
                    c.clause,
                    Clause::From | Clause::Join | Clause::On | Clause::Using | Clause::Target
                ) {
                    self.ctx_mut().from = FromSt::None;
                    Some(i + 1)
                } else {
                    None
                }
            }
            "ON" => {
                if c.stmt == Stmt::Insert && self.w(i + 1, "CONFLICT") {
                    self.set_clause(Clause::Other);
                    let ctx = self.ctx_mut();
                    ctx.from = FromSt::None;
                    ctx.column_list_for = ctx.target;
                    return Some(i + 2);
                }
                if c.stmt == Stmt::Insert && self.w(i + 1, "DUPLICATE") {
                    self.set_clause(Clause::Set);
                    self.ctx_mut().from = FromSt::None;
                    let mut j = i + 2;
                    while j < (i + 5).min(self.t.len()) && self.any(j, &["KEY", "UPDATE"]) {
                        j += 1;
                    }
                    return Some(j);
                }
                if matches!(c.clause, Clause::From | Clause::Join | Clause::Target) {
                    self.set_clause(Clause::On);
                    self.ctx_mut().from = FromSt::None;
                    Some(i + 1)
                } else {
                    None
                }
            }
            "USING" => {
                if c.stmt == Stmt::Delete || c.stmt == Stmt::Merge {
                    self.set_clause(Clause::From);
                    self.ctx_mut().from = FromSt::Expect(RefRole::Source);
                    Some(i + 1)
                } else if c.clause == Clause::Join
                    || matches!(c.from, FromSt::After(_) | FromSt::AfterAlias(_))
                {
                    self.set_clause(Clause::Using);
                    self.ctx_mut().from = FromSt::None;
                    Some(i + 1)
                } else {
                    None
                }
            }
            "WHERE" | "QUALIFY" | "PREWHERE" => {
                self.close_item(&c, i);
                self.set_clause(Clause::Where);
                self.ctx_mut().from = FromSt::None;
                Some(i + 1)
            }
            "GROUP" | "ORDER" if self.w(i + 1, "BY") => {
                self.close_item(&c, i);
                self.set_clause(if word == "GROUP" {
                    Clause::GroupBy
                } else {
                    Clause::OrderBy
                });
                self.ctx_mut().from = FromSt::None;
                Some(i + 2)
            }
            "HAVING" => {
                self.close_item(&c, i);
                self.set_clause(Clause::Having);
                Some(i + 1)
            }
            "WINDOW" => {
                self.close_item(&c, i);
                self.set_clause(Clause::Window);
                self.ctx_mut().from = FromSt::None;
                Some(i + 1)
            }
            "LIMIT" | "OFFSET" | "FETCH" => {
                self.close_item(&c, i);
                self.set_clause(Clause::Limit);
                self.ctx_mut().from = FromSt::None;
                Some(i + 1)
            }
            "RETURNING" | "OUTPUT" => {
                self.close_item(&c, i);
                self.set_clause(Clause::Returning);
                self.ctx_mut().from = FromSt::None;
                Some(i + 1)
            }
            "INTO" => {
                if c.clause == Clause::Select {
                    self.close_item(&c, i);
                    self.set_clause(Clause::Into);
                    return Some(i + 1);
                }
                if matches!(c.stmt, Stmt::Insert | Stmt::Merge) && c.target.is_none() {
                    self.set_clause(Clause::Target);
                    self.ctx_mut().from = FromSt::Expect(RefRole::Target);
                    return Some(i + 1);
                }
                None
            }
            "SET" => {
                if matches!(c.stmt, Stmt::Update | Stmt::Merge | Stmt::Insert) {
                    self.set_clause(Clause::Set);
                    self.ctx_mut().from = FromSt::None;
                    Some(i + 1)
                } else {
                    None
                }
            }
            "VALUES" => {
                if matches!(c.stmt, Stmt::Insert | Stmt::Merge) {
                    self.set_clause(Clause::Values);
                    self.ctx_mut().from = FromSt::None;
                    Some(i + 1)
                } else {
                    None
                }
            }
            "INSERT" if c.stmt == Stmt::Merge => {
                let ctx = self.ctx_mut();
                ctx.column_list_for = ctx.target;
                ctx.from = FromSt::None;
                Some(i + 1)
            }
            "UNION" | "INTERSECT" | "EXCEPT" | "MINUS" => Some(self.new_branch(i)),
            "WHEN" | "THEN" if c.stmt == Stmt::Merge => {
                self.set_clause(Clause::Other);
                self.ctx_mut().from = FromSt::None;
                Some(i + 1)
            }
            "FOR"
                if matches!(
                    c.clause,
                    Clause::From
                        | Clause::Join
                        | Clause::On
                        | Clause::Where
                        | Clause::OrderBy
                        | Clause::Limit
                        | Clause::GroupBy
                        | Clause::Having
                ) =>
            {
                self.close_item(&c, i);
                self.set_clause(Clause::Other);
                self.ctx_mut().from = FromSt::None;
                Some(i + 1)
            }
            "BEGIN"
                if !self.any(
                    i + 1,
                    &[
                        "TRANSACTION",
                        "TRAN",
                        "WORK",
                        "ISOLATION",
                        "READ",
                        "DEFERRED",
                        "IMMEDIATE",
                        "EXCLUSIVE",
                    ],
                ) =>
            {
                Some(self.new_statement(i))
            }
            "REFERENCES" => Some(self.references(i)),
            _ => None,
        }
    }

    fn scope_token(&mut self, i: usize) -> usize {
        let t = self.t[i];
        let word = self.upper(i);
        let started = self.ctx().map(|c| c.started).unwrap_or(true);
        if !started {
            self.ctx_mut().started = true;
            if !word.is_empty()
                && let Some(next) = self.dispatch(i, &word)
            {
                return next;
            }
        }
        let Some(c) = self.ctx().cloned() else {
            return i + 1;
        };
        if t.kind == TokenKind::Semicolon {
            return self.new_statement(i);
        }
        // WITH list
        match c.cte {
            CteSt::ExpectName if t.kind.is_word() => {
                let name = self.ident(i).unwrap_or_default();
                let id = self.m.ctes.len();
                self.m.ctes.push(Cte {
                    name,
                    columns: Vec::new(),
                    body: None,
                    name_start: t.start,
                    name_end: t.end,
                });
                if let Some(s) = self.m.scopes.get_mut(c.id) {
                    s.ctes.push(id);
                }
                self.ctx_mut().cte = CteSt::AfterName(id);
                return i + 1;
            }
            CteSt::AfterName(id) if self.w(i, "AS") => {
                let mut j = i + 1;
                while self.any(j, &["NOT", "MATERIALIZED"]) {
                    j += 1;
                }
                self.ctx_mut().cte = CteSt::ExpectBody(id);
                return j;
            }
            CteSt::AfterBody if t.kind.is_word() && !self.any(i, &["SEARCH", "CYCLE"]) => {
                // Main statement after the WITH list.
                let ctx = self.ctx_mut();
                ctx.cte = CteSt::None;
                if let Some(next) = self.dispatch(i, &word) {
                    return next;
                }
            }
            _ => {}
        }
        // Table reference states.
        match c.from {
            FromSt::Expect(role) => {
                if self.refish(i)
                    || matches!(t.kind, TokenKind::Parameter | TokenKind::Variable)
                    || self.any(i, &["LATERAL", "ONLY"])
                {
                    return self.parse_ref(i, role);
                }
            }
            FromSt::After(s)
                if self
                    .m
                    .sources
                    .get(s)
                    .is_some_and(|x| matches!(x.role, RefRole::Ddl | RefRole::Create)) =>
            {
                // DDL references take no aliases (`DROP TABLE t CASCADE`).
                self.ctx_mut().from = FromSt::None;
            }
            FromSt::After(s) => {
                if self.w(i, "AS") {
                    self.ctx_mut().from = FromSt::AfterAs(s);
                    return i + 1;
                }
                if self.nameish(i) {
                    let alias = self.ident(i);
                    if let Some(src) = self.m.sources.get_mut(s) {
                        src.alias = alias;
                    }
                    self.ctx_mut().from = FromSt::AfterAlias(s);
                    return i + 1;
                }
                self.ctx_mut().from = FromSt::None;
            }
            FromSt::AfterAs(s) => {
                if t.kind.is_word() {
                    let alias = self.ident(i);
                    if let Some(src) = self.m.sources.get_mut(s) {
                        src.alias = alias;
                    }
                    self.ctx_mut().from = FromSt::AfterAlias(s);
                    return i + 1;
                }
                self.ctx_mut().from = FromSt::None;
            }
            FromSt::AfterAlias(_) => {
                self.ctx_mut().from = FromSt::None;
            }
            FromSt::None => {}
        }
        if matches!(t.kind, TokenKind::Keyword | TokenKind::Identifier)
            && let Some(next) = self.clause_word(i, &word)
        {
            return next;
        }
        if t.kind == TokenKind::Comma {
            self.comma(i);
            return i + 1;
        }
        // Expression token in the current clause.
        let c = self.ctx().cloned();
        if let Some(c) = &c
            && c.clause == Clause::Select
            && c.item_start.is_none()
        {
            self.ctx_mut().item_start = Some(i);
        }
        if t.kind.is_word() {
            return self.expr_word(i);
        }
        i + 1
    }
}

// ---------------------------------------------------------------- public API

/// Analyze one statement (offsets are relative to `stmt`).
pub fn analyze(stmt: &str, syntax: &SqlSyntax) -> StatementAnalysis {
    let tokens = tokenize(stmt, syntax);
    let m = build_model(stmt, &tokens, None, None, stmt.len());
    to_public(&m)
}

fn to_public(m: &Model) -> StatementAnalysis {
    let mut out = StatementAnalysis::default();
    for s in &m.sources {
        match &s.kind {
            SourceKind::Table {
                catalog,
                schema,
                name,
            } => out.tables.push(TableReference {
                catalog: catalog.clone(),
                schema: schema.clone(),
                name: name.clone(),
                alias: s.alias.clone(),
                role: s.role,
                is_cte: false,
                start: s.start,
                end: s.end,
                scope: s.scope,
            }),
            SourceKind::Cte(c) => out.tables.push(TableReference {
                catalog: None,
                schema: None,
                name: m.ctes.get(*c).map(|c| c.name.clone()).unwrap_or_default(),
                alias: s.alias.clone(),
                role: s.role,
                is_cte: true,
                start: s.start,
                end: s.end,
                scope: s.scope,
            }),
            _ => {}
        }
    }
    for c in &m.ctes {
        let columns = if !c.columns.is_empty() {
            c.columns.clone()
        } else {
            c.body.map(|b| m.scope_columns(b)).unwrap_or_default()
        };
        let (start, end) = c
            .body
            .and_then(|b| m.scopes.get(b))
            .map(|s| (s.start, s.end))
            .unwrap_or((c.name_end, c.name_end));
        out.ctes.push(CteInfo {
            name: c.name.clone(),
            columns,
            name_start: c.name_start,
            name_end: c.name_end,
            start,
            end,
        });
    }
    for (i, s) in m.scopes.iter().enumerate() {
        if !matches!(s.kind, ScopeKind::Subquery | ScopeKind::Derived) {
            continue;
        }
        let derived = s.kind == ScopeKind::Derived;
        let src = m.sources.iter().find(|x| x.kind == SourceKind::Derived(i));
        let alias = src.and_then(|x| x.alias.clone());
        let columns = match src {
            Some(x) if !x.alias_columns.is_empty() => x.alias_columns.clone(),
            _ => m.scope_columns(i),
        };
        out.subqueries.push(SubqueryInfo {
            alias,
            columns,
            start: s.start,
            end: s.end,
            scope: i,
            derived,
        });
    }
    out
}

/// The table or column under `offset` in a script, resolving aliases, for
/// cmd+click navigation. `None` on keywords, literals, CTE names, derived
/// table aliases and unresolvable names.
pub fn object_at(src: &str, offset: usize, syntax: &SqlSyntax) -> Option<ObjectReference> {
    let split = split_tokens(src, syntax, &SplitOptions::default());
    let spans: Vec<_> = split.statements.iter().map(|s| s.span).collect();
    let span = pick_statement(&spans, src, offset)?;
    let st = split.statements.iter().find(|s| s.span == span)?;
    let toks = split.tokens.get(st.tokens.clone())?;
    let tok = toks
        .iter()
        .find(|t| t.start <= offset && offset < t.end || (t.end == offset && t.kind.is_word()))?;
    if !tok.kind.is_word() {
        return None;
    }
    let m = build_model(src, toks, None, None, span.end);
    let table_of = |s: &Source| -> Option<(Option<String>, String)> {
        match &s.kind {
            SourceKind::Table { schema, name, .. } => Some((schema.clone(), name.clone())),
            _ => None,
        }
    };
    // On a table name (or its schema part) / alias definition.
    for s in &m.sources {
        if s.parts.iter().any(|p| p.1 <= tok.start && tok.end <= p.2) {
            let (schema, table) = table_of(s)?;
            return Some(ObjectReference {
                schema,
                table,
                column: None,
                start: tok.start,
                end: tok.end,
            });
        }
    }
    // On a column reference.
    for c in &m.cols {
        let Some(pos) = c
            .parts
            .iter()
            .position(|p| p.1 == tok.start && p.2 == tok.end)
        else {
            continue;
        };
        if c.call {
            return None;
        }
        if let Some(s) = c.implicit {
            let (schema, table) = table_of(m.sources.get(s)?)?;
            return Some(ObjectReference {
                schema,
                table,
                column: Some(c.parts[pos].0.clone()),
                start: tok.start,
                end: tok.end,
            });
        }
        let n = c.parts.len();
        if n == 1 {
            // Unqualified column: only when exactly one table is in scope.
            let vis: Vec<usize> = m
                .visible_sources(c.scope)
                .into_iter()
                .filter(|(i, d)| *d == 0 && m.sources[*i].in_column_scope())
                .map(|(i, _)| i)
                .collect();
            if vis.len() != 1 {
                return None;
            }
            let (schema, table) = table_of(&m.sources[vis[0]])?;
            return Some(ObjectReference {
                schema,
                table,
                column: Some(c.parts[0].0.clone()),
                start: tok.start,
                end: tok.end,
            });
        }
        let (qual, column) = (&c.parts[..n - 1], &c.parts[n - 1]);
        let source = if qual.len() == 1 {
            m.resolve_qualifier(c.scope, &qual[0].0)
        } else {
            None
        };
        let (schema, table) = match source {
            Some(s) => table_of(&m.sources[s])?,
            None => {
                // schema.table.column without a FROM entry
                let table = qual.last()?.0.clone();
                let schema = if qual.len() >= 2 {
                    Some(qual[qual.len() - 2].0.clone())
                } else {
                    None
                };
                (schema, table)
            }
        };
        if pos == n - 1 {
            return Some(ObjectReference {
                schema,
                table,
                column: Some(column.0.clone()),
                start: tok.start,
                end: tok.end,
            });
        }
        return Some(ObjectReference {
            schema,
            table,
            column: None,
            start: tok.start,
            end: tok.end,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    fn tables(a: &StatementAnalysis) -> Vec<(Option<&str>, &str, Option<&str>, RefRole)> {
        a.tables
            .iter()
            .map(|t| {
                (
                    t.schema.as_deref(),
                    t.name.as_str(),
                    t.alias.as_deref(),
                    t.role,
                )
            })
            .collect()
    }

    #[test]
    fn select_sources_and_aliases() {
        let a = analyze(
            "SELECT u.id FROM public.users u JOIN orders AS o ON o.user_id = u.id, \"Items\" i WHERE 1 = 1",
            &pg(),
        );
        assert_eq!(
            tables(&a),
            vec![
                (Some("public"), "users", Some("u"), RefRole::Source),
                (None, "orders", Some("o"), RefRole::Source),
                (None, "Items", Some("i"), RefRole::Source),
            ]
        );
        let a = analyze(
            "SELECT * FROM a LEFT OUTER JOIN b ON a.x = b.x NATURAL JOIN c CROSS JOIN d WHERE x",
            &pg(),
        );
        let names: Vec<_> = a
            .tables
            .iter()
            .map(|t| (t.name.as_str(), t.alias.as_deref()))
            .collect();
        assert_eq!(
            names,
            vec![("a", None), ("b", None), ("c", None), ("d", None)]
        );
    }

    #[test]
    fn ctes_and_columns() {
        let a = analyze(
            "WITH x(a, b) AS (SELECT 1, 2), y AS (SELECT id, name AS n, count(*) cnt, u.* FROM users u) SELECT * FROM x JOIN y ON true",
            &pg(),
        );
        assert_eq!(a.ctes.len(), 2);
        assert_eq!(a.ctes[0].name, "x");
        assert_eq!(a.ctes[0].columns, vec!["a", "b"]);
        assert_eq!(a.ctes[1].columns, vec!["id", "n", "cnt"]);
        let refs: Vec<_> = a
            .tables
            .iter()
            .map(|t| (t.name.as_str(), t.is_cte))
            .collect();
        assert_eq!(refs, vec![("users", false), ("x", true), ("y", true)]);
        // recursive CTE references itself
        let a = analyze(
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 5) SELECT n FROM t",
            &pg(),
        );
        assert!(a.tables.iter().all(|t| t.is_cte));
    }

    #[test]
    fn derived_tables() {
        let a = analyze(
            "SELECT s.a FROM (SELECT a, b AS bee, t.c FROM t) s JOIN (SELECT 1 x) AS q(y) ON true",
            &pg(),
        );
        assert_eq!(a.subqueries.len(), 2);
        assert_eq!(a.subqueries[0].alias.as_deref(), Some("s"));
        assert_eq!(a.subqueries[0].columns, vec!["a", "bee", "c"]);
        assert!(a.subqueries[0].derived);
        assert_eq!(a.subqueries[1].alias.as_deref(), Some("q"));
        assert_eq!(a.subqueries[1].columns, vec!["y"]);
        let a = analyze(
            "SELECT * FROM users WHERE id IN (SELECT user_id FROM orders)",
            &pg(),
        );
        assert_eq!(a.subqueries.len(), 1);
        assert!(!a.subqueries[0].derived);
        assert_eq!(a.tables[1].scope, a.subqueries[0].scope);
    }

    #[test]
    fn dml_targets() {
        let t = |q: &str| {
            tables(&analyze(q, &pg()))
                .into_iter()
                .map(|x| (x.1.to_string(), x.2.map(str::to_string), x.3))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            t("INSERT INTO s.t (a, b) SELECT x, y FROM u"),
            vec![
                ("t".into(), None, RefRole::Target),
                ("u".into(), None, RefRole::Source)
            ]
        );
        assert_eq!(
            t("UPDATE users u SET name = 'x' FROM orders o WHERE o.user_id = u.id"),
            vec![
                ("users".into(), Some("u".into()), RefRole::Target),
                ("orders".into(), Some("o".into()), RefRole::Source)
            ]
        );
        assert_eq!(
            t("DELETE FROM users AS u USING orders o WHERE o.id = u.id"),
            vec![
                ("users".into(), Some("u".into()), RefRole::Target),
                ("orders".into(), Some("o".into()), RefRole::Source)
            ]
        );
        assert_eq!(
            t("MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET v = s.v"),
            vec![
                ("t".into(), None, RefRole::Target),
                ("s".into(), None, RefRole::Source)
            ]
        );
        assert_eq!(
            t("TABLE users"),
            vec![("users".into(), None, RefRole::Source)]
        );
    }

    #[test]
    fn ddl_references() {
        let t = |q: &str| {
            tables(&analyze(q, &pg()))
                .into_iter()
                .map(|x| (x.1.to_string(), x.3))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            t("ALTER TABLE IF EXISTS users ADD COLUMN x int REFERENCES orders (id)"),
            vec![
                ("users".into(), RefRole::Ddl),
                ("orders".into(), RefRole::Ddl)
            ]
        );
        assert_eq!(
            t("DROP TABLE IF EXISTS a, b CASCADE"),
            vec![("a".into(), RefRole::Ddl), ("b".into(), RefRole::Ddl)]
        );
        assert_eq!(t("TRUNCATE TABLE a"), vec![("a".into(), RefRole::Ddl)]);
        assert_eq!(
            t("CREATE TABLE n (id int, u int REFERENCES users(id))"),
            vec![
                ("n".into(), RefRole::Create),
                ("users".into(), RefRole::Ddl)
            ]
        );
        assert_eq!(
            t("CREATE VIEW v AS SELECT * FROM users"),
            vec![
                ("v".into(), RefRole::Create),
                ("users".into(), RefRole::Source)
            ]
        );
        assert_eq!(
            t("CREATE UNIQUE INDEX i ON users (email)"),
            vec![("users".into(), RefRole::Ddl)]
        );
        assert_eq!(
            t("EXPLAIN ANALYZE SELECT * FROM users"),
            vec![("users".into(), RefRole::Source)]
        );
        assert_eq!(
            t("COMMENT ON TABLE users IS 'x'"),
            vec![("users".into(), RefRole::Ddl)]
        );
        assert_eq!(
            t("LOCK TABLE users IN SHARE MODE"),
            vec![("users".into(), RefRole::Ddl)]
        );
        assert_eq!(
            t("GRANT SELECT ON TABLE users, orders TO bob"),
            vec![
                ("users".into(), RefRole::Ddl),
                ("orders".into(), RefRole::Ddl)
            ]
        );
        assert_eq!(t("DESCRIBE users"), vec![("users".into(), RefRole::Ddl)]);
    }

    #[test]
    fn unions_and_compound_bodies() {
        let a = analyze(
            "SELECT a FROM t1 UNION ALL SELECT b FROM t2 ORDER BY 1",
            &pg(),
        );
        let scopes: Vec<_> = a.tables.iter().map(|t| t.scope).collect();
        assert_ne!(scopes[0], scopes[1]);
        let a = analyze(
            "CREATE PROCEDURE p() BEGIN UPDATE a SET x = 1; SELECT * FROM b; END",
            &mysql(),
        );
        let names: Vec<_> = a.tables.iter().map(|t| (t.name.as_str(), t.role)).collect();
        assert_eq!(names, vec![("a", RefRole::Target), ("b", RefRole::Source)]);
    }

    #[test]
    fn mysql_and_tsql_quirks() {
        let a = analyze(
            "SELECT * FROM `shop`.`order items` oi USE INDEX (idx) STRAIGHT_JOIN b",
            &mysql(),
        );
        assert_eq!(
            tables(&a)[0],
            (Some("shop"), "order items", Some("oi"), RefRole::Source)
        );
        assert_eq!(a.tables[1].name, "b");
        let a = analyze(
            "SELECT TOP 5 * FROM [dbo].[Users] AS u WITH (NOLOCK) WHERE u.id = 1",
            &tsql(),
        );
        assert_eq!(
            tables(&a),
            vec![(Some("dbo"), "Users", Some("u"), RefRole::Source)]
        );
        let a = analyze(
            "SELECT * FROM generate_series(1, 10) g(n), unnest(arr) WITH ORDINALITY",
            &pg(),
        );
        assert!(a.tables.is_empty());
    }

    #[test]
    fn object_at_resolves_aliases() {
        let src = "SELECT 1;\nSELECT u.name, o.total, missing.x FROM app.users u JOIN orders o ON o.user_id = u.id;";
        let at =
            |needle: &str, delta: usize| object_at(src, src.find(needle).unwrap() + delta, &pg());
        let r = at("u.name", 3).unwrap();
        assert_eq!(
            (r.schema.as_deref(), r.table.as_str(), r.column.as_deref()),
            (Some("app"), "users", Some("name"))
        );
        let r = at("u.name", 0).unwrap();
        assert_eq!((r.table.as_str(), r.column.as_deref()), ("users", None));
        let r = at("o.total", 2).unwrap();
        assert_eq!(
            (r.table.as_str(), r.column.as_deref()),
            ("orders", Some("total"))
        );
        let r = at("app.users", 1).unwrap();
        assert_eq!(
            (r.schema.as_deref(), r.table.as_str()),
            (Some("app"), "users")
        );
        let r = at("users u", 2).unwrap();
        assert_eq!(r.table, "users");
        assert_eq!(r.end - r.start, 5);
        // `missing.x`: qualifier not in FROM → table guess `missing`
        let r = at("missing.x", 9).unwrap();
        assert_eq!(
            (r.table.as_str(), r.column.as_deref()),
            ("missing", Some("x"))
        );
        assert!(at("SELECT u", 2).is_none());
        assert!(object_at(src, 7, &pg()).is_none()); // the literal 1
        // single table: unqualified column
        let src = "SELECT name FROM users WHERE id = 1";
        let r = object_at(src, 8, &pg()).unwrap();
        assert_eq!(
            (r.table.as_str(), r.column.as_deref()),
            ("users", Some("name"))
        );
        // INSERT column list
        let src = "INSERT INTO users (email) VALUES ('x')";
        let r = object_at(src, 20, &pg()).unwrap();
        assert_eq!(
            (r.table.as_str(), r.column.as_deref()),
            ("users", Some("email"))
        );
        // functions and CTEs are not objects
        assert!(object_at("SELECT count(*) FROM t", 9, &pg()).is_none());
        let src = "WITH x AS (SELECT 1 a) SELECT * FROM x";
        assert!(object_at(src, src.len() - 1, &pg()).is_none());
    }

    #[test]
    fn cursor_snapshot() {
        let src = "SELECT  FROM users u WHERE ";
        let toks = tokenize(src, &pg());
        let m = build_model(src, &toks, Some(7), None, src.len());
        let c = m.cursor.unwrap();
        assert_eq!(c.clause, Clause::Select);
        let m = build_model(src, &toks, Some(src.len()), None, src.len());
        assert_eq!(m.cursor.unwrap().clause, Clause::Where);
        let src = "SELECT * FROM ";
        let toks = tokenize(src, &pg());
        let m = build_model(src, &toks, Some(src.len()), None, src.len());
        assert_eq!(m.cursor.unwrap().from, FromSt::Expect(RefRole::Source));
    }

    #[test]
    fn random_inputs_never_panic() {
        let mut rng = Rng::new(23);
        for _ in 0..400 {
            let src = random_sql(&mut rng, 40);
            for syn in all_syntaxes() {
                let a = analyze(&src, &syn);
                for t in &a.tables {
                    assert!(t.start <= t.end && t.end <= src.len());
                }
                let off = floor_boundary(&src, rng.below(src.len() + 1));
                let _ = object_at(&src, off, &syn);
                let toks = tokenize(&src, &syn);
                let _ = build_model(&src, &toks, Some(off), None, src.len());
            }
        }
    }
}
