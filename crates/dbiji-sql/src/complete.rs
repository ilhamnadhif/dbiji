//! Schema-aware autocomplete.
//!
//! [`complete`] analyses only the statement around the cursor and tolerates
//! incomplete SQL. It proposes, depending on the position:
//!
//! * after `FROM` / `JOIN` / `UPDATE` / `INTO` / `TABLE` / `DESCRIBE` ...:
//!   tables and views of the default schema, CTEs and schemas; after
//!   `JOIN` tables related by foreign keys come first, together with full
//!   join proposals (`orders o ON o.user_id = u.id`);
//! * after `schema.`: that schema's tables; after `alias.` / `table.`: the
//!   columns of that source (aliases are resolved anywhere in the statement,
//!   CTE and derived-table columns included);
//! * in expressions (select list, `WHERE`, `ON`, `GROUP BY`, `ORDER BY`,
//!   `HAVING`, `SET` ...): columns of the tables in scope, aliases,
//!   functions and position-appropriate keywords; right after `ON`, join
//!   conditions derived from foreign keys;
//! * column names inside `INSERT INTO t (`, after `UPDATE t SET`, in
//!   `CREATE INDEX ... ON t (` and `REFERENCES t (`;
//! * statement keywords at the start of a statement, and variables after
//!   `${`, `:` and `@`.
//!
//! Exact and prefix matches (case-insensitive) always rank above fuzzy
//! subsequence matches; identifiers that need quotes are inserted quoted.

use crate::KeywordCase;
use crate::analyze::{
    Clause, CteSt, CursorCtx, FromSt, Model, ParenKind, RefRole, ScopeKind, SourceKind, Stmt,
    build_model,
};
use crate::catalog::CompletionCatalog;
use crate::commands::{ClientCommand, parse_client_command};
use crate::fuzzy::fuzzy_score;
use crate::lexicon::{ident_eq, reserved, word_set};
use crate::position::floor_char_boundary;
use crate::split::{BlankLineMode, SplitOptions, split_tokens};
use crate::token::{Token, TokenKind, is_word, unquote_ident};
use dbiji_core::ddl::ForeignKeyDef;
use dbiji_core::meta::ColumnBrief;
use dbiji_core::{Dialect, ObjectKind, SqlSyntax, tr, trf};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

/// What a completion item inserts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CompletionKind {
    /// SQL keyword.
    Keyword,
    /// Schema name.
    Schema,
    /// Table.
    Table,
    /// View or materialized view.
    View,
    /// Column of a table in scope.
    Column,
    /// Table alias in scope.
    Alias,
    /// Function (inserted with parentheses).
    Function,
    /// Common table expression.
    Cte,
    /// A join proposal or join condition derived from foreign keys.
    Join,
    /// Statement template.
    Snippet,
    /// Script variable or parameter (`${x}`, `:x`, `@x`).
    Variable,
}

/// One proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    /// Text shown in the list.
    pub label: String,
    /// Text inserted in place of `replace_start..replace_end`.
    pub insert: String,
    /// What the item is.
    pub kind: CompletionKind,
    /// Greyed extra text (table and type of a column, schema of a table ...).
    pub detail: Option<String>,
    /// Higher is better; items are sorted by it.
    pub score: i32,
    /// Characters to move the cursor back after inserting (inside `()`).
    pub cursor_back: usize,
}

/// Proposals and the source range they replace.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompletionResult {
    /// Start of the replaced range (byte offset).
    pub replace_start: usize,
    /// End of the replaced range (byte offset).
    pub replace_end: usize,
    /// Proposals, best first.
    pub items: Vec<CompletionItem>,
}

/// Autocomplete settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompletionOptions {
    /// Maximum number of items returned.
    pub max_items: usize,
    /// Case of inserted keywords.
    pub keyword_case: KeywordCase,
    /// Offer keywords.
    pub include_keywords: bool,
    /// Insert an alias after table names in `FROM` / `JOIN` (`users u`).
    pub insert_aliases: bool,
    /// Insert columns qualified with their table alias when several tables are in scope.
    pub qualify_columns: bool,
    /// How the script is split to find the statement around the cursor.
    pub split: SplitOptions,
}

impl Default for CompletionOptions {
    fn default() -> Self {
        CompletionOptions {
            max_items: 100,
            keyword_case: KeywordCase::Upper,
            include_keywords: true,
            insert_aliases: true,
            qualify_columns: false,
            split: SplitOptions::default(),
        }
    }
}

// Score tiers: exact > prefix > fuzzy, independent of the context bonus (<= 700).
const EXACT: i32 = 3000;
const PREFIX: i32 = 2000;
const FUZZY_MAX: i32 = 900;

fn match_score(prefix: &str, label: &str) -> Option<i32> {
    if prefix.is_empty() {
        return Some(0);
    }
    let p = prefix.to_lowercase();
    let l = label.to_lowercase();
    if l == p {
        return Some(EXACT);
    }
    if l.starts_with(&p) {
        let extra = l.chars().count().saturating_sub(p.chars().count()).min(200) as i32;
        return Some(PREFIX + 200 - extra);
    }
    fuzzy_score(&p, label).map(|s| (s * 3).clamp(0, FUZZY_MAX))
}

const STATEMENT_START: &[&str] = &[
    "SELECT",
    "INSERT INTO",
    "UPDATE",
    "DELETE FROM",
    "WITH",
    "CREATE TABLE",
    "CREATE VIEW",
    "CREATE INDEX",
    "ALTER TABLE",
    "DROP TABLE",
    "TRUNCATE TABLE",
    "EXPLAIN",
    "SET",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "GRANT",
    "REVOKE",
    "CALL",
    "VALUES",
    "MERGE INTO",
];
const STATEMENT_START_OPTIONAL: &[&str] = &[
    "SHOW", "DESCRIBE", "USE", "PRAGMA", "VACUUM", "ANALYZE", "COPY", "OPTIMIZE",
];
const SUBQUERY_START: &[&str] = &["SELECT", "WITH", "VALUES"];
const AFTER_FROM_ITEM: &[&str] = &[
    "WHERE",
    "JOIN",
    "INNER JOIN",
    "LEFT JOIN",
    "RIGHT JOIN",
    "FULL JOIN",
    "CROSS JOIN",
    "GROUP BY",
    "ORDER BY",
    "HAVING",
    "LIMIT",
    "UNION",
    "UNION ALL",
    "AS",
];
const AFTER_JOIN_ITEM: &[&str] = &[
    "ON",
    "USING",
    "AS",
    "WHERE",
    "JOIN",
    "INNER JOIN",
    "LEFT JOIN",
    "GROUP BY",
    "ORDER BY",
];
const EXPR_START: &[&str] = &["CASE", "NOT", "EXISTS", "NULL", "TRUE", "FALSE"];
const CONDITION_AFTER: &[&str] = &[
    "AND",
    "OR",
    "IS NULL",
    "IS NOT NULL",
    "IN",
    "NOT IN",
    "LIKE",
    "BETWEEN",
    "GROUP BY",
    "ORDER BY",
    "LIMIT",
    "UNION",
];
const ON_AFTER: &[&str] = &[
    "AND",
    "OR",
    "JOIN",
    "LEFT JOIN",
    "INNER JOIN",
    "WHERE",
    "GROUP BY",
    "ORDER BY",
];
const SELECT_AFTER: &[&str] = &["FROM", "AS"];
const GROUP_AFTER: &[&str] = &["HAVING", "ORDER BY", "LIMIT"];
const ORDER_AFTER: &[&str] = &[
    "ASC",
    "DESC",
    "NULLS FIRST",
    "NULLS LAST",
    "LIMIT",
    "OFFSET",
];
const LIMIT_AFTER: &[&str] = &["OFFSET"];
const SET_AFTER: &[&str] = &["WHERE", "FROM", "RETURNING"];
const AFTER_CTE: &[&str] = &[
    "SELECT",
    "INSERT INTO",
    "UPDATE",
    "DELETE FROM",
    "MERGE INTO",
];
const INSERT_AFTER_TARGET: &[&str] = &["VALUES", "SELECT", "DEFAULT VALUES"];
const DELETE_AFTER_TARGET: &[&str] = &["WHERE", "USING"];
const CLIENT_COMMANDS: &[&str] = &["@set", "@include", "@export", "@echo"];
/// Keywords that end an operand-less position (`SELECT *` is an operand).
const VALUE_KEYWORDS: &[&str] = &[
    "NULL",
    "TRUE",
    "FALSE",
    "END",
    "CURRENT_DATE",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "CURRENT_USER",
];
/// Non-reserved keywords that are never operands.
const NOT_OPERANDS: &[&str] = &[
    "ILIKE",
    "INTERVAL",
    "OVER",
    "PARTITION",
    "FILTER",
    "WITHIN",
    "ESCAPE",
    "RETURNING",
    "ROWS",
    "RANGE",
    "NULLS",
    "SET",
    "VALUES",
    "EXPLAIN",
    "ANALYZE",
    "SHOW",
    "TABLE",
    "INDEX",
    "VIEW",
    "UPDATE",
    "DELETE",
    "INSERT",
    "REPLACE",
];

struct ColInfo {
    name: String,
    data_type: Option<String>,
    table: String,
}

#[derive(Clone, Copy)]
enum VarStyle {
    Template,
    Named,
    At,
}

struct Completer<'a> {
    src: &'a str,
    dialect: &'a dyn Dialect,
    syntax: &'a SqlSyntax,
    catalog: &'a dyn CompletionCatalog,
    opts: &'a CompletionOptions,
    model: Model,
    ctx: CursorCtx,
    prefix: String,
    quote: Option<(char, char)>,
    prev: Vec<Token>,
    replace_end: usize,
    items: Vec<CompletionItem>,
    col_cache: RefCell<HashMap<TableKey, Option<Vec<ColumnBrief>>>>,
    fk_cache: RefCell<HashMap<TableKey, Vec<ForeignKeyDef>>>,
}

/// Lower-case (schema, table) cache key.
type TableKey = (Option<String>, String);

/// Completion proposals for `cursor` (byte offset) in `src`.
pub fn complete(
    src: &str,
    cursor: usize,
    dialect: &dyn Dialect,
    catalog: &dyn CompletionCatalog,
    opts: &CompletionOptions,
) -> CompletionResult {
    let cursor = floor_char_boundary(src, cursor);
    let syntax = dialect.syntax();
    let split = split_tokens(src, syntax, &opts.split);
    let tokens = &split.tokens;
    let idx = tokens.partition_point(|t| t.end < cursor);
    let at = tokens
        .get(idx)
        .copied()
        .filter(|t| t.start < cursor && cursor <= t.end);
    let mut result = CompletionResult {
        replace_start: cursor,
        replace_end: cursor,
        items: Vec::new(),
    };
    let mut prefix = String::new();
    let mut quote = None;
    let mut var_style = None;
    if let Some(t) = at {
        let inside_end = cursor < t.end || t.unterminated;
        match t.kind {
            TokenKind::LineComment => return result,
            TokenKind::BlockComment | TokenKind::String if inside_end => return result,
            TokenKind::Number => return result,
            TokenKind::Keyword | TokenKind::Identifier => {
                result.replace_start = t.start;
                result.replace_end = t.end;
                prefix = src[t.start..cursor].to_string();
            }
            TokenKind::QuotedIdentifier => {
                let text = t.text(src);
                let open = text.chars().next().unwrap_or('"');
                let close = match open {
                    '[' => ']',
                    c => c,
                };
                quote = Some((open, close));
                result.replace_start = t.start;
                result.replace_end = if t.unterminated { cursor } else { t.end };
                let typed = &src[t.start + open.len_utf8()..cursor];
                prefix = typed.trim_end_matches(close).to_string();
            }
            TokenKind::Variable => {
                let text = &src[t.start..cursor];
                result.replace_start = t.start;
                result.replace_end = t.end;
                if let Some(p) = text.strip_prefix("${") {
                    prefix = p.trim_end_matches('}').to_string();
                    var_style = Some(VarStyle::Template);
                } else {
                    prefix = text.trim_start_matches('@').to_string();
                    var_style = Some(VarStyle::At);
                }
            }
            TokenKind::Parameter if t.text(src).starts_with(':') => {
                result.replace_start = t.start;
                result.replace_end = t.end;
                prefix = src[t.start + 1..cursor].to_string();
                var_style = Some(VarStyle::Named);
            }
            TokenKind::ClientCommand => {
                let text = t.text(src);
                let word_end = t.start
                    + 1
                    + text[1..]
                        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                        .unwrap_or(text.len() - 1);
                if cursor <= word_end {
                    result.replace_start = t.start;
                    result.replace_end = word_end;
                    let typed = &src[t.start..cursor];
                    for c in CLIENT_COMMANDS {
                        if let Some(score) = match_score(typed, c) {
                            result.items.push(CompletionItem {
                                label: (*c).to_string(),
                                insert: format!("{c} "),
                                kind: CompletionKind::Snippet,
                                detail: Some(tr("client command").to_string()),
                                score: score + 600,
                                cursor_back: 0,
                            });
                        }
                    }
                    finish(&mut result.items, opts.max_items);
                }
                return result;
            }
            _ => {}
        }
    }
    let rs = result.replace_start;
    // Statement around the cursor.
    let stmts = &split.statements;
    let chosen = stmts
        .iter()
        .find(|s| s.span.start <= rs && rs <= s.span.end && !s.span.is_client_command)
        .or_else(|| {
            let p = stmts.iter().rev().find(|s| s.span.end < rs)?;
            if p.span.terminated || p.span.is_client_command {
                return None;
            }
            // A blank line between the statement and the cursor starts a new one.
            let gap = src.get(p.span.end..rs).unwrap_or("");
            let lines: Vec<&str> = gap.split('\n').collect();
            let blank = lines.len() >= 3
                && lines[1..lines.len() - 1]
                    .iter()
                    .any(|l| l.trim().is_empty());
            if blank && opts.split.blank_line != BlankLineMode::Never {
                None
            } else {
                Some(p)
            }
        });
    let (stmt_tokens, stmt_end): (&[Token], usize) = match chosen {
        Some(s) => (
            tokens.get(s.tokens.clone()).unwrap_or(&[]),
            s.span.outer_end.max(cursor),
        ),
        None => (&[], cursor),
    };
    // The word being typed is not part of the analysed statement.
    let skip = (rs < result.replace_end).then_some((rs, result.replace_end));
    let model = build_model(src, stmt_tokens, Some(cursor), skip, stmt_end);
    let Some(ctx) = model.cursor else {
        return result;
    };
    let prev: Vec<Token> = stmt_tokens
        .iter()
        .filter(|t| !t.is_trivia() && t.end <= rs)
        .copied()
        .collect();
    let mut c = Completer {
        src,
        dialect,
        syntax,
        catalog,
        opts,
        model,
        ctx,
        prefix,
        quote,
        prev,
        replace_end: result.replace_end,
        items: Vec::new(),
        col_cache: RefCell::new(HashMap::new()),
        fk_cache: RefCell::new(HashMap::new()),
    };
    match var_style {
        Some(style) => c.variables(style, &split.tokens),
        None => c.run(),
    }
    let mut items = c.items;
    finish(&mut items, opts.max_items);
    result.items = items;
    result
}

/// Deduplicate, sort and truncate.
fn finish(items: &mut Vec<CompletionItem>, max: usize) {
    items.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.label.len().cmp(&b.label.len()))
            .then_with(|| a.label.cmp(&b.label))
    });
    let mut seen: HashSet<(String, CompletionKind, String)> = HashSet::with_capacity(items.len());
    items.retain(|i| seen.insert((i.label.to_lowercase(), i.kind, i.insert.clone())));
    items.truncate(max.max(1));
}

impl Completer<'_> {
    fn push(
        &mut self,
        label: &str,
        insert: String,
        kind: CompletionKind,
        detail: Option<String>,
        bonus: i32,
        cursor_back: usize,
    ) {
        let Some(m) = match_score(&self.prefix, label) else {
            return;
        };
        self.items.push(CompletionItem {
            label: label.to_string(),
            insert,
            kind,
            detail,
            score: m + bonus,
            cursor_back,
        });
    }

    fn keyword(&mut self, kw: &str, bonus: i32) {
        if !self.opts.include_keywords {
            return;
        }
        let text = self.opts.keyword_case.apply(kw);
        self.push(&text, text.clone(), CompletionKind::Keyword, None, bonus, 0);
    }

    /// Keywords of a context; earlier entries rank slightly higher.
    fn keywords(&mut self, list: &[&str], bonus: i32) {
        for (i, k) in list.iter().enumerate() {
            self.keyword(k, bonus - i.min(40) as i32);
        }
    }

    /// Dialect keywords matching a non-empty prefix, with a low score.
    fn all_keywords(&mut self) {
        if self.prefix.is_empty() || !self.opts.include_keywords {
            return;
        }
        for k in self.syntax.keywords {
            self.keyword(k, 20);
        }
    }

    fn quote_ident(&self, name: &str) -> String {
        match self.quote {
            Some((open, close)) => {
                let mut s = String::with_capacity(name.len() + 2);
                s.push(open);
                for ch in name.chars() {
                    if ch == close {
                        s.push(close);
                    }
                    s.push(ch);
                }
                s.push(close);
                s
            }
            None => self.dialect.quote_ident(name),
        }
    }

    fn default_schema(&self) -> Option<String> {
        self.catalog.default_schema()
    }

    fn columns(&self, schema: Option<&str>, table: &str) -> Option<Vec<ColumnBrief>> {
        let key = (schema.map(str::to_lowercase), table.to_lowercase());
        if let Some(c) = self.col_cache.borrow().get(&key) {
            return c.clone();
        }
        let cols = self.catalog.columns(schema, table);
        self.col_cache.borrow_mut().insert(key, cols.clone());
        cols
    }

    fn fks(&self, schema: Option<&str>, table: &str) -> Vec<ForeignKeyDef> {
        let key = (schema.map(str::to_lowercase), table.to_lowercase());
        if let Some(c) = self.fk_cache.borrow().get(&key) {
            return c.clone();
        }
        let fks = self.catalog.foreign_keys(schema, table);
        self.fk_cache.borrow_mut().insert(key, fks.clone());
        fks
    }

    // ------------------------------------------------------------ columns

    fn source_columns(&self, si: usize, depth: u32) -> Vec<ColInfo> {
        let Some(src) = self.model.sources.get(si) else {
            return Vec::new();
        };
        let label = src.qualifier().unwrap_or("").to_string();
        let mut cols: Vec<ColInfo> = match &src.kind {
            SourceKind::Table { schema, name, .. } => self
                .columns(schema.as_deref(), name)
                .unwrap_or_default()
                .into_iter()
                .map(|c| ColInfo {
                    name: c.name,
                    data_type: Some(c.data_type).filter(|t| !t.is_empty()),
                    table: name.clone(),
                })
                .collect(),
            SourceKind::Cte(c) => self.cte_columns(*c, depth),
            SourceKind::Derived(scope) => self.scope_output(*scope, depth),
            SourceKind::Function(_) | SourceKind::Unknown => Vec::new(),
        };
        if !src.alias_columns.is_empty() {
            let n = src.alias_columns.len().max(cols.len());
            let mut renamed = Vec::with_capacity(n);
            for (i, a) in src.alias_columns.iter().enumerate() {
                let dt = cols.get(i).and_then(|c| c.data_type.clone());
                renamed.push(ColInfo {
                    name: a.clone(),
                    data_type: dt,
                    table: label.clone(),
                });
            }
            renamed.extend(cols.drain(..).skip(src.alias_columns.len()));
            cols = renamed;
        }
        if !matches!(src.kind, SourceKind::Table { .. }) {
            for c in &mut cols {
                c.table = label.clone();
            }
        }
        cols
    }

    fn cte_columns(&self, c: usize, depth: u32) -> Vec<ColInfo> {
        let Some(cte) = self.model.ctes.get(c) else {
            return Vec::new();
        };
        if !cte.columns.is_empty() {
            return cte
                .columns
                .iter()
                .map(|n| ColInfo {
                    name: n.clone(),
                    data_type: None,
                    table: cte.name.clone(),
                })
                .collect();
        }
        match cte.body {
            Some(b) if depth < 6 => self.scope_output(b, depth + 1),
            _ => Vec::new(),
        }
    }

    /// Output columns of a query scope, expanding `*` / `t.*`.
    fn scope_output(&self, scope: usize, depth: u32) -> Vec<ColInfo> {
        if depth > 6 {
            return Vec::new();
        }
        let scope = self
            .model
            .scopes
            .get(scope)
            .and_then(|s| s.union_first)
            .unwrap_or(scope);
        let Some(sc) = self.model.scopes.get(scope) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for it in &sc.items {
            match &it.star {
                Some(None) => {
                    for &si in &sc.sources {
                        if self.model.sources[si].in_column_scope() {
                            out.extend(self.source_columns(si, depth + 1));
                        }
                    }
                }
                Some(Some(q)) => {
                    if let Some(si) = self.model.resolve_qualifier(scope, q) {
                        out.extend(self.source_columns(si, depth + 1));
                    }
                }
                None => {
                    if let Some(n) = &it.name {
                        out.push(ColInfo {
                            name: n.clone(),
                            data_type: None,
                            table: String::new(),
                        });
                    }
                }
            }
        }
        out
    }

    fn column_detail(c: &ColInfo) -> Option<String> {
        match (&c.data_type, c.table.is_empty()) {
            (Some(t), false) => Some(trf("{0}: {1}", &[&c.table, t])),
            (Some(t), true) => Some(t.clone()),
            (None, false) => Some(c.table.clone()),
            (None, true) => None,
        }
    }

    fn push_columns(&mut self, si: usize, bonus: i32, qualify: bool) {
        let cols = self.source_columns(si, 0);
        let qual = self
            .model
            .sources
            .get(si)
            .and_then(|s| s.qualifier())
            .map(str::to_string);
        for col in cols {
            let name = self.quote_ident(&col.name);
            let insert = match (&qual, qualify) {
                (Some(q), true) => format!("{}.{}", self.dialect.quote_ident(q), name),
                _ => name,
            };
            let detail = Self::column_detail(&col);
            self.push(&col.name, insert, CompletionKind::Column, detail, bonus, 0);
        }
    }

    /// Whether a source contributes unqualified columns in the current context.
    fn source_visible(&self, si: usize) -> bool {
        let Some(s) = self.model.sources.get(si) else {
            return false;
        };
        match s.role {
            RefRole::Source => true,
            RefRole::Target => match self.ctx.stmt {
                Stmt::Insert => matches!(
                    self.ctx.clause,
                    Clause::Set | Clause::Returning | Clause::Other
                ),
                _ => true,
            },
            RefRole::Ddl | RefRole::Create => {
                self.ctx.stmt == Stmt::Ddl && self.ctx.target == Some(si)
            }
        }
    }

    fn scope_columns(&mut self, bonus: i32) {
        let vis = self.model.visible_sources(self.ctx.scope);
        let own: Vec<usize> = vis
            .iter()
            .filter(|(i, d)| *d == 0 && self.source_visible(*i))
            .map(|(i, _)| *i)
            .collect();
        let qualify = self.opts.qualify_columns && own.len() >= 2;
        for (si, dist) in vis {
            if !self.source_visible(si) {
                continue;
            }
            self.push_columns(si, bonus - 100 * dist.min(3) as i32, qualify || dist > 0);
        }
        if let Some(t) = self.ctx.target
            && self.ctx.stmt == Stmt::Ddl
            && !self
                .model
                .scopes
                .get(self.ctx.scope)
                .is_some_and(|s| s.sources.contains(&t))
        {
            self.push_columns(t, bonus, false);
        }
    }

    fn aliases(&mut self, bonus: i32) {
        let vis = self.model.visible_sources(self.ctx.scope);
        for (si, dist) in vis {
            if !self.source_visible(si) {
                continue;
            }
            let s = &self.model.sources[si];
            let Some(q) = s.qualifier().map(str::to_string) else {
                continue;
            };
            let detail = match &s.kind {
                SourceKind::Table { name, .. } if s.alias.is_some() => {
                    Some(trf("alias of {0}", &[name]))
                }
                SourceKind::Table { .. } => Some(tr("table").to_string()),
                SourceKind::Cte(_) => Some(tr("CTE").to_string()),
                SourceKind::Derived(_) => Some(tr("subquery").to_string()),
                _ => None,
            };
            let insert = self.quote_ident(&q);
            self.push(
                &q,
                insert,
                CompletionKind::Alias,
                detail,
                bonus - 100 * dist.min(3) as i32,
                0,
            );
        }
    }

    fn functions(&mut self, bonus: i32) {
        let paren_follows = self.src[self.replace_end..].starts_with('(');
        let mut names: Vec<String> = self
            .syntax
            .functions
            .iter()
            .map(|f| f.to_string())
            .collect();
        names.extend(self.catalog.functions(None));
        for f in names {
            let (insert, back) = if paren_follows {
                (f.clone(), 0)
            } else {
                (format!("{f}()"), 1)
            };
            self.push(
                &f,
                insert,
                CompletionKind::Function,
                Some(tr("function").to_string()),
                bonus,
                back,
            );
        }
    }

    // ------------------------------------------------------------ tables

    fn taken_names(&self) -> Vec<String> {
        let mut taken = Vec::new();
        for s in &self.model.sources {
            if let Some(a) = &s.alias {
                taken.push(a.to_lowercase());
            }
            if let Some(p) = s.parts.last() {
                taken.push(p.0.to_lowercase());
            }
        }
        taken
    }

    fn make_alias(&self, name: &str, taken: &[String]) -> String {
        let mut base = String::new();
        let mut prev: Option<char> = None;
        for ch in name.chars() {
            let boundary = match prev {
                None => true,
                Some(p) => {
                    (!p.is_alphanumeric() && ch.is_alphanumeric())
                        || (p.is_lowercase() && ch.is_uppercase())
                }
            };
            if boundary && ch.is_alphabetic() {
                base.extend(ch.to_lowercase());
            }
            prev = Some(ch);
        }
        if base.is_empty() || !base.chars().all(|c| c.is_ascii_alphabetic()) {
            base = "t".into();
        }
        let res = reserved();
        let kw = word_set(self.syntax.keywords, false);
        let bad = |a: &str| taken.iter().any(|t| t == a) || res.contains(a) || kw.contains(a);
        if !bad(&base) {
            return base;
        }
        (2..100)
            .map(|n| format!("{base}{n}"))
            .find(|a| !bad(a))
            .unwrap_or(base)
    }

    fn view_like(kind: ObjectKind) -> bool {
        matches!(kind, ObjectKind::View | ObjectKind::MaterializedView)
    }

    /// Own-scope table sources: (source index, schema, name, qualifier).
    fn scope_tables(&self) -> Vec<(usize, Option<String>, String, String)> {
        let Some(sc) = self.model.scopes.get(self.ctx.scope) else {
            return Vec::new();
        };
        sc.sources
            .iter()
            .filter_map(|&si| {
                let s = &self.model.sources[si];
                if !matches!(s.role, RefRole::Source | RefRole::Target) {
                    return None;
                }
                match &s.kind {
                    SourceKind::Table { schema, name, .. } => Some((
                        si,
                        schema.clone(),
                        name.clone(),
                        s.qualifier().unwrap_or(name).to_string(),
                    )),
                    _ => None,
                }
            })
            .collect()
    }

    fn schema_matches(&self, a: Option<&str>, b: Option<&str>) -> bool {
        let d = self.default_schema();
        let a = a.or(d.as_deref());
        let b = b.or(d.as_deref());
        match (a, b) {
            (Some(a), Some(b)) => ident_eq(a, b),
            _ => true,
        }
    }

    /// Join conditions `new.col = other.col` from foreign keys between a new
    /// table (schema, name, qualifier) and an existing source.
    fn conditions(
        &self,
        new: (Option<&str>, &str, &str),
        other: (Option<&str>, &str, &str),
    ) -> Vec<String> {
        let (ns, nn, nq) = new;
        let (os, on, oq) = other;
        let mut fks = self.fks(ns, nn);
        for fk in self.fks(os, on) {
            if !fks.contains(&fk) {
                fks.push(fk);
            }
        }
        let q = |s: &str| self.dialect.quote_ident(s);
        let mut out: Vec<String> = Vec::new();
        for fk in &fks {
            let fk_table = if fk.table.is_empty() {
                nn
            } else {
                fk.table.as_str()
            };
            let from_new = ident_eq(fk_table, nn) && self.schema_matches(fk.schema.as_deref(), ns);
            let to_other =
                ident_eq(&fk.ref_table, on) && self.schema_matches(fk.ref_schema.as_deref(), os);
            let from_other =
                ident_eq(fk_table, on) && self.schema_matches(fk.schema.as_deref(), os);
            let to_new =
                ident_eq(&fk.ref_table, nn) && self.schema_matches(fk.ref_schema.as_deref(), ns);
            let mut add = |pairs: Vec<(String, String)>| {
                if pairs.is_empty() {
                    return;
                }
                let cond = pairs
                    .iter()
                    .map(|(a, b)| format!("{}.{} = {}.{}", q(nq), q(a), q(oq), q(b)))
                    .collect::<Vec<_>>()
                    .join(" AND ");
                if !out.contains(&cond) {
                    out.push(cond);
                }
            };
            if from_new && to_other {
                add(fk
                    .columns
                    .iter()
                    .cloned()
                    .zip(fk.ref_columns.iter().cloned())
                    .collect());
            }
            if from_other && to_new && !(nq == oq) {
                add(fk
                    .ref_columns
                    .iter()
                    .cloned()
                    .zip(fk.columns.iter().cloned())
                    .collect());
            }
        }
        out
    }

    fn table_context(&mut self, role: RefRole) {
        let join = self.ctx.clause == Clause::Join;
        let alias_ok = self.opts.insert_aliases
            && role == RefRole::Source
            && matches!(self.ctx.clause, Clause::From | Clause::Join);
        let taken = self.taken_names();
        // CTEs
        for c in self.model.visible_ctes(self.ctx.scope) {
            let name = self.model.ctes[c].name.clone();
            let insert = self.quote_ident(&name);
            self.push(
                &name,
                insert,
                CompletionKind::Cte,
                Some(tr("CTE").to_string()),
                650,
                0,
            );
        }
        // Tables related by foreign keys to the tables already in scope.
        let scope_tables = self.scope_tables();
        let mut related: Vec<(Option<String>, String)> = Vec::new();
        if join {
            for (_, s, n, _) in &scope_tables {
                for fk in self.fks(s.as_deref(), n) {
                    let fk_table = if fk.table.is_empty() {
                        n.clone()
                    } else {
                        fk.table.clone()
                    };
                    for (rs, rn) in [
                        (fk.schema.clone(), fk_table),
                        (fk.ref_schema.clone(), fk.ref_table.clone()),
                    ] {
                        if !related.iter().any(|(a, b)| {
                            ident_eq(b, &rn) && self.schema_matches(a.as_deref(), rs.as_deref())
                        }) {
                            related.push((rs, rn));
                        }
                    }
                }
            }
        }
        let default = self.default_schema();
        for t in self.catalog.tables(None) {
            let kind = if Self::view_like(t.kind) {
                CompletionKind::View
            } else {
                CompletionKind::Table
            };
            let is_related = related.iter().any(|(s, n)| {
                ident_eq(n, &t.name) && self.schema_matches(s.as_deref(), t.schema.as_deref())
            });
            let detail = t.comment.clone().filter(|c| !c.is_empty()).or_else(|| {
                Some(if Self::view_like(t.kind) {
                    tr("view").to_string()
                } else {
                    tr("table").to_string()
                })
            });
            let alias = alias_ok.then(|| self.make_alias(&t.name, &taken));
            let name = self.quote_ident(&t.name);
            let insert = match &alias {
                Some(a) => format!("{name} {a}"),
                None => name.clone(),
            };
            // A partition after its table (`api_calls` before `api_calls_2026_01`).
            let score = match (is_related, t.kind) {
                (true, _) => 680,
                (false, ObjectKind::Partition) => 590,
                _ => 600,
            };
            self.push(&t.name, insert, kind, detail, score, 0);
        }
        // Full join proposals, including related tables of other schemas.
        if join {
            for (rs, rn) in related.clone() {
                let other_schema = rs
                    .as_deref()
                    .is_some_and(|s| default.as_deref().is_none_or(|d| !ident_eq(s, d)));
                let alias = if self.opts.insert_aliases {
                    self.make_alias(&rn, &taken)
                } else {
                    rn.clone()
                };
                let table_text = if other_schema {
                    self.dialect.qualified(rs.as_deref(), &rn)
                } else {
                    self.dialect.quote_ident(&rn)
                };
                for (_, s, n, q) in &scope_tables {
                    for cond in self.conditions((rs.as_deref(), &rn, &alias), (s.as_deref(), n, q))
                    {
                        let head = if self.opts.insert_aliases {
                            format!("{table_text} {}", self.dialect.quote_ident(&alias))
                        } else {
                            table_text.clone()
                        };
                        let text = format!("{head} ON {cond}");
                        self.push_join(&text, &rn, 700);
                    }
                }
            }
        }
        // Schemas
        for s in self.catalog.schemas() {
            let insert = self.quote_ident(&s);
            self.push(
                &s,
                insert,
                CompletionKind::Schema,
                Some(tr("schema").to_string()),
                400,
                0,
            );
        }
        if self.ctx.clause == Clause::From || join {
            self.keywords(&["LATERAL"], 50);
        }
        self.all_keywords();
    }

    /// Join items match the prefix against the table name, not the condition text.
    fn push_join(&mut self, text: &str, table: &str, bonus: i32) {
        let Some(m) = match_score(&self.prefix, table).or_else(|| match_score(&self.prefix, text))
        else {
            return;
        };
        self.items.push(CompletionItem {
            label: text.to_string(),
            insert: text.to_string(),
            kind: CompletionKind::Join,
            detail: Some(tr("join").to_string()),
            score: m + bonus,
            cursor_back: 0,
        });
    }

    fn tables_of_schema(&mut self, schema: &str) {
        for t in self.catalog.tables(Some(schema)) {
            let kind = if Self::view_like(t.kind) {
                CompletionKind::View
            } else {
                CompletionKind::Table
            };
            let insert = self.quote_ident(&t.name);
            let detail = Some(schema.to_string());
            self.push(&t.name, insert, kind, detail, 600, 0);
        }
    }

    fn is_schema(&self, name: &str) -> bool {
        self.catalog.schemas().iter().any(|s| ident_eq(s, name))
    }

    // ------------------------------------------------------------ contexts

    fn after_operand(&self) -> bool {
        let n = self.prev.len();
        let Some(last) = self.prev.last() else {
            return false;
        };
        match last.kind {
            TokenKind::Identifier
            | TokenKind::QuotedIdentifier
            | TokenKind::Number
            | TokenKind::String
            | TokenKind::RParen
            | TokenKind::Parameter
            | TokenKind::Variable => true,
            // Non-reserved keywords are usually column names (`name`, `type`).
            TokenKind::Keyword => {
                VALUE_KEYWORDS.iter().any(|w| is_word(last, self.src, w))
                    || (!reserved().contains(last.text(self.src))
                        && !NOT_OPERANDS.iter().any(|w| is_word(last, self.src, w)))
            }
            TokenKind::Operator if last.text(self.src) == "*" => {
                n < 2 || {
                    let p = &self.prev[n - 2];
                    matches!(
                        p.kind,
                        TokenKind::Comma | TokenKind::Dot | TokenKind::LParen
                    ) || is_word(p, self.src, "SELECT")
                        || is_word(p, self.src, "DISTINCT")
                }
            }
            _ => false,
        }
    }

    fn last_is(&self, word: &str) -> bool {
        self.prev.last().is_some_and(|t| is_word(t, self.src, word))
    }

    fn last_kind(&self) -> Option<TokenKind> {
        self.prev.last().map(|t| t.kind)
    }

    /// `a.b.` before the cursor: the qualifier parts.
    fn qualifier_chain(&self) -> Option<(Vec<String>, usize)> {
        let n = self.prev.len();
        if n < 2 || self.prev[n - 1].kind != TokenKind::Dot {
            return None;
        }
        let mut parts = Vec::new();
        let mut i = n - 1; // index of a dot
        loop {
            if i == 0 {
                break;
            }
            let w = &self.prev[i - 1];
            if !w.kind.is_word() {
                break;
            }
            parts.push(unquote_ident(w.text(self.src)).into_owned());
            if i >= 2 && self.prev[i - 2].kind == TokenKind::Dot && parts.len() < 3 {
                i -= 2;
            } else {
                i -= 1;
                break;
            }
        }
        if parts.is_empty() {
            return None;
        }
        parts.reverse();
        let first_tok = self.prev.get(i.min(n - 1)).map(|t| t.start).unwrap_or(0);
        Some((parts, first_tok))
    }

    fn qualified(&mut self, parts: &[String], first_start: usize) {
        let in_table_position =
            self.model.sources.iter().any(|s| {
                s.start == first_start || s.parts.first().is_some_and(|p| p.1 == first_start)
            });
        let scope = self.ctx.scope;
        if in_table_position {
            let schema = parts.last().cloned().unwrap_or_default();
            self.tables_of_schema(&schema);
            return;
        }
        match parts {
            [q] => {
                if let Some(si) = self.model.resolve_qualifier(scope, q) {
                    self.push_columns(si, 600, false);
                    return;
                }
                if let Some(c) = self.model.find_cte(scope, q) {
                    for col in self.cte_columns(c, 0) {
                        let insert = self.quote_ident(&col.name);
                        let detail = Self::column_detail(&col);
                        self.push(&col.name, insert, CompletionKind::Column, detail, 600, 0);
                    }
                    return;
                }
                if self.is_schema(q) {
                    self.tables_of_schema(q);
                    for f in self.catalog.functions(Some(q)) {
                        let back = usize::from(!self.src[self.replace_end..].starts_with('('));
                        let insert = if back == 1 {
                            format!("{f}()")
                        } else {
                            f.clone()
                        };
                        self.push(
                            &f,
                            insert,
                            CompletionKind::Function,
                            Some(tr("function").to_string()),
                            300,
                            back,
                        );
                    }
                    return;
                }
                if let Some(t) = self.catalog.find_table(None, q) {
                    self.table_columns(None, &t.name, 600);
                }
            }
            [s, t] => {
                if self.catalog.find_table(Some(s), t).is_some() {
                    self.table_columns(Some(s), t, 600);
                } else if self.is_schema(t) {
                    self.tables_of_schema(t);
                }
            }
            [_, s, t] => self.table_columns(Some(s), t, 600),
            _ => {}
        }
    }

    fn table_columns(&mut self, schema: Option<&str>, table: &str, bonus: i32) {
        for c in self.columns(schema, table).unwrap_or_default() {
            let insert = self.quote_ident(&c.name);
            let info = ColInfo {
                name: c.name.clone(),
                data_type: Some(c.data_type.clone()),
                table: table.to_string(),
            };
            let detail = Self::column_detail(&info);
            self.push(&c.name, insert, CompletionKind::Column, detail, bonus, 0);
        }
    }

    fn statement_start(&mut self) {
        let root = self
            .model
            .scopes
            .get(self.ctx.scope)
            .is_some_and(|s| s.kind == ScopeKind::Root);
        if root {
            self.keywords(STATEMENT_START, 600);
            let kw = word_set(self.syntax.keywords, false);
            for k in STATEMENT_START_OPTIONAL {
                if kw.contains(k) {
                    self.keyword(k, 580);
                }
            }
            let snippet = format!(
                "{} * {} ",
                self.opts.keyword_case.apply("SELECT"),
                self.opts.keyword_case.apply("FROM")
            );
            self.push(
                snippet.trim_end(),
                snippet.clone(),
                CompletionKind::Snippet,
                Some(tr("snippet").to_string()),
                550,
                0,
            );
        } else {
            self.keywords(SUBQUERY_START, 600);
        }
        self.all_keywords();
    }

    fn after_table_ref(&mut self, si: usize) {
        let role = self
            .model
            .sources
            .get(si)
            .map(|s| s.role)
            .unwrap_or(RefRole::Source);
        match (self.ctx.stmt, role) {
            (Stmt::Insert, RefRole::Target) => self.keywords(INSERT_AFTER_TARGET, 600),
            (Stmt::Update, RefRole::Target) => self.keywords(&["SET", "AS"], 600),
            (Stmt::Delete, RefRole::Target) => self.keywords(DELETE_AFTER_TARGET, 600),
            (Stmt::Merge, RefRole::Target) => self.keywords(&["USING", "AS"], 600),
            _ if self.ctx.clause == Clause::Join => {
                self.join_conditions_for(si, true);
                self.keywords(AFTER_JOIN_ITEM, 600);
            }
            _ => self.keywords(AFTER_FROM_ITEM, 600),
        }
        self.all_keywords();
    }

    /// Join conditions between source `si` and the other own-scope tables.
    fn join_conditions_for(&mut self, si: usize, with_on: bool) {
        let Some(src) = self.model.sources.get(si) else {
            return;
        };
        let SourceKind::Table { schema, name, .. } = &src.kind else {
            return;
        };
        let (schema, name) = (schema.clone(), name.clone());
        let q = src.qualifier().unwrap_or(&name).to_string();
        let others: Vec<_> = self
            .scope_tables()
            .into_iter()
            .filter(|(i, ..)| *i != si)
            .collect();
        let on = self.opts.keyword_case.apply("ON");
        for (_, os, on_, oq) in others {
            if ident_eq(&oq, &q) {
                continue;
            }
            for cond in self.conditions((schema.as_deref(), &name, &q), (os.as_deref(), &on_, &oq))
            {
                let text = if with_on {
                    format!("{on} {cond}")
                } else {
                    cond
                };
                let Some(m) = match_score(&self.prefix, &text) else {
                    continue;
                };
                self.items.push(CompletionItem {
                    label: text.clone(),
                    insert: text,
                    kind: CompletionKind::Join,
                    detail: Some(tr("join condition").to_string()),
                    score: m + 700,
                    cursor_back: 0,
                });
            }
        }
    }

    fn last_source_in_scope(&self) -> Option<usize> {
        self.model
            .scopes
            .get(self.ctx.scope)?
            .sources
            .iter()
            .rev()
            .copied()
            .find(|&i| self.model.sources[i].role == RefRole::Source)
    }

    fn expression(&mut self) {
        let operand = self.after_operand();
        let clause = self.ctx.clause;
        let continuation: &[&str] = match clause {
            Clause::Select => SELECT_AFTER,
            Clause::Where | Clause::Having => CONDITION_AFTER,
            Clause::On => ON_AFTER,
            Clause::GroupBy => GROUP_AFTER,
            Clause::OrderBy => ORDER_AFTER,
            Clause::Limit => LIMIT_AFTER,
            Clause::Set => SET_AFTER,
            _ => &[],
        };
        if operand {
            self.keywords(continuation, 650);
            self.scope_columns(300);
            self.functions(200);
        } else {
            self.scope_columns(600);
            self.aliases(550);
            self.functions(400);
            self.keywords(EXPR_START, 200);
            if clause == Clause::Select && self.last_is("SELECT") {
                self.keywords(&["DISTINCT", "*"], 250);
            }
            if matches!(clause, Clause::Where | Clause::Having | Clause::On)
                && self.last_kind().is_some_and(|k| k == TokenKind::Operator)
            {
                self.keywords(&["NULL"], 200);
            }
        }
        self.all_keywords();
    }

    fn run(&mut self) {
        let c = self.ctx;
        if let Some((parts, first)) = self.qualifier_chain() {
            self.qualified(&parts, first);
            return;
        }
        match c.paren {
            Some(ParenKind::Columns(s)) => {
                if self.last_kind() == Some(TokenKind::LParen)
                    || self.last_kind() == Some(TokenKind::Comma)
                {
                    self.push_columns(s, 600, false);
                }
                return;
            }
            Some(ParenKind::UsingColumns) => {
                let tables = self.scope_tables();
                for (si, ..) in tables.iter().rev().take(2) {
                    self.push_columns(*si, 600, false);
                }
                return;
            }
            Some(ParenKind::CteColumns(_)) | Some(ParenKind::AliasColumns(_)) => return,
            _ => {}
        }
        match c.cte {
            CteSt::ExpectName => return,
            CteSt::AfterName(_) => {
                self.keyword("AS", 600);
                return;
            }
            CteSt::ExpectBody(_) => {
                self.keywords(&["MATERIALIZED", "NOT MATERIALIZED"], 300);
                return;
            }
            CteSt::AfterBody => {
                self.keywords(AFTER_CTE, 600);
                return;
            }
            CteSt::None => {}
        }
        if c.paren == Some(ParenKind::JoinGroup) && self.last_kind() == Some(TokenKind::LParen) {
            // `FROM (|`: a derived table or a parenthesised join.
            self.keywords(SUBQUERY_START, 700);
            self.table_context(RefRole::Source);
            return;
        }
        if let FromSt::Expect(role) = c.from {
            self.table_context(role);
            return;
        }
        match c.from {
            FromSt::AfterAs(_) => return,
            FromSt::After(s) | FromSt::AfterAlias(s) => {
                self.after_table_ref(s);
                return;
            }
            _ => {}
        }
        if !c.started {
            self.statement_start();
            return;
        }
        // Utility statements naming a table: `ALTER TABLE t ALTER COLUMN |`.
        if c.stmt == Stmt::Ddl && self.last_is("COLUMN") {
            if let Some(t) = c.target {
                self.push_columns(t, 600, false);
            }
            return;
        }
        match c.clause {
            Clause::Start => {
                match c.stmt {
                    Stmt::Insert | Stmt::Merge => self.keyword("INTO", 600),
                    Stmt::Delete => self.keyword("FROM", 600),
                    _ => {}
                }
                self.all_keywords();
            }
            Clause::On if self.last_is("ON") => {
                if let Some(si) = self.last_source_in_scope() {
                    self.join_conditions_for(si, false);
                }
                self.expression();
            }
            Clause::Set
                if self.last_is("SET")
                    || self.last_kind() == Some(TokenKind::Comma)
                    || self.last_is("UPDATE") =>
            {
                if let Some(t) = c.target {
                    self.push_columns(t, 600, false);
                }
                self.all_keywords();
            }
            Clause::Into | Clause::With => self.all_keywords(),
            _ => self.expression(),
        }
    }

    fn variables(&mut self, style: VarStyle, tokens: &[Token]) {
        let mut names: Vec<String> = Vec::new();
        let mut add = |n: &str| {
            if !n.is_empty() && !names.iter().any(|x| x == n) {
                names.push(n.to_string());
            }
        };
        for t in tokens {
            let text = t.text(self.src);
            match t.kind {
                TokenKind::ClientCommand => {
                    if let Some(Ok(ClientCommand::Set { name, .. })) = parse_client_command(text) {
                        add(&name);
                    }
                }
                TokenKind::Variable if text.starts_with("${") && !t.unterminated => {
                    add(text.trim_start_matches("${").trim_end_matches('}'))
                }
                TokenKind::Variable if matches!(style, VarStyle::At) => {
                    add(text.trim_start_matches('@'))
                }
                TokenKind::Parameter if text.starts_with(':') => add(&text[1..]),
                _ => {}
            }
        }
        for n in names {
            let insert = match style {
                VarStyle::Template => format!("${{{n}}}"),
                VarStyle::Named => format!(":{n}"),
                VarStyle::At => format!("@{n}"),
            };
            self.push(
                &n,
                insert,
                CompletionKind::Variable,
                Some(tr("variable").to_string()),
                600,
                0,
            );
        }
    }
}

#[cfg(test)]
mod tests;
