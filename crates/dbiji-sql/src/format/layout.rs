//! Statement layout: splits a statement into clauses (SELECT, FROM, JOIN,
//! WHERE ...) and builds a [`Doc`] with one clause per line, lists broken one
//! item per line when they do not fit (or always without compact lists),
//! conditions broken at AND/OR, subqueries and CTE bodies indented, CASE
//! expressions expanded when long and function calls kept compact.

use super::FormatOptions;
use super::doc::Doc;
use crate::token::TokenKind;
use std::cell::Cell;

/// A comment attached to a significant token.
#[derive(Debug, Clone)]
pub(crate) struct Note {
    pub text: String,
    /// Line comment (must be followed by a line break).
    pub line: bool,
    /// A line break precedes the comment in the source.
    pub nl_before: bool,
    /// A line break follows the comment in the source.
    pub nl_after: bool,
}

/// Significant token prepared for layout.
#[derive(Debug, Clone)]
pub(crate) struct FTok {
    pub kind: TokenKind,
    /// Output text (keyword case applied).
    pub text: String,
    /// Upper-case text of words (for keyword matching), empty otherwise.
    pub upper: String,
    pub lead: Vec<Note>,
    pub trail: Vec<Note>,
    /// No white space or comment between this token and the previous one in the source.
    pub glued: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Keyword followed by inline text (`LIMIT 10`, `UPDATE t`).
    Inline,
    /// Comma separated items (`SELECT`, `GROUP BY`, `SET` ...).
    List,
    /// Conditions split at AND / OR.
    Cond,
    Join,
    SetOp,
    With,
}

#[derive(Debug, Clone, Copy)]
struct Clause {
    kind: Kind,
    kw: (usize, usize),
    body: (usize, usize),
}

const SPACE_BEFORE_PAREN: &[&str] = &[
    "IN",
    "VALUES",
    "AS",
    "ON",
    "USING",
    "EXISTS",
    "AND",
    "OR",
    "NOT",
    "FROM",
    "JOIN",
    "WHERE",
    "SELECT",
    "INTO",
    "OVER",
    "FILTER",
    "GROUP",
    "ALL",
    "ANY",
    "SOME",
    "UNION",
    "INTERSECT",
    "EXCEPT",
    "THEN",
    "ELSE",
    "WHEN",
    "RETURNING",
    "KEY",
    "UNIQUE",
    "CHECK",
    "DEFAULT",
    "BY",
    "LATERAL",
    "MATERIALIZED",
    "TABLE",
    "WITH",
    "SET",
    "HAVING",
    "CONFLICT",
    "LIKE",
    "IS",
    "BETWEEN",
    "PARTITION",
    "RECURSIVE",
    "OUTPUT",
    "INCLUDE",
    "PRIMARY",
    "FOREIGN",
    "REFERENCES",
];
const VALUE_KEYWORDS: &[&str] = &[
    "NULL",
    "TRUE",
    "FALSE",
    "END",
    "CURRENT_DATE",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
];
const JOIN_WORDS: &[&str] = &[
    "NATURAL", "LEFT", "RIGHT", "FULL", "INNER", "OUTER", "CROSS", "SEMI", "ANTI", "ASOF",
    "GLOBAL", "ANY",
];
/// Binary operators after which a unary `-`/`+` may be separated by a space.
const SAFE_BEFORE_UNARY: &[&str] = &["=", "<", ">", "<=", ">=", "<>", "*", "/", "=>"];
const QUERY_WORDS: &[&str] = &[
    "SELECT", "WITH", "VALUES", "INSERT", "UPDATE", "DELETE", "MERGE", "REPLACE", "UPSERT", "TABLE",
];

pub(crate) struct Layout<'a> {
    t: &'a [FTok],
    opts: &'a FormatOptions,
    close: Vec<usize>,
    case_end: Vec<usize>,
    /// Current nesting of parentheses / CASE being laid out.
    depth: Cell<u32>,
}

/// Deeper nesting is laid out inline (keeps recursion bounded).
const MAX_NEST: u32 = 40;

impl<'a> Layout<'a> {
    pub(crate) fn new(t: &'a [FTok], opts: &'a FormatOptions) -> Layout<'a> {
        let n = t.len();
        let mut close = vec![usize::MAX; n];
        let mut case_end = vec![usize::MAX; n];
        let mut parens: Vec<usize> = Vec::new();
        let mut cases: Vec<(usize, usize)> = Vec::new(); // (case index, paren depth)
        for (i, tok) in t.iter().enumerate() {
            match tok.kind {
                TokenKind::LParen => parens.push(i),
                TokenKind::RParen => {
                    if let Some(o) = parens.pop() {
                        close[o] = i;
                    }
                    // CASE without END inside the closed parentheses stays unmatched.
                    while cases.last().is_some_and(|&(_, d)| d > parens.len()) {
                        cases.pop();
                    }
                }
                TokenKind::Keyword | TokenKind::Identifier => {
                    if tok.upper == "CASE" {
                        cases.push((i, parens.len()));
                    } else if tok.upper == "END"
                        && let Some(&(c, d)) = cases.last()
                        && d == parens.len()
                    {
                        case_end[c] = i;
                        cases.pop();
                    }
                }
                _ => {}
            }
        }
        Layout {
            t,
            opts,
            close,
            case_end,
            depth: Cell::new(0),
        }
    }

    fn up(&self, i: usize) -> &str {
        self.t.get(i).map(|t| t.upper.as_str()).unwrap_or("")
    }

    fn is_op(&self, i: usize, op: &str) -> bool {
        self.t
            .get(i)
            .is_some_and(|t| t.kind == TokenKind::Operator && t.text == op)
    }

    /// Doc of one token with its comments.
    fn tok(&self, i: usize) -> Doc {
        let Some(t) = self.t.get(i) else {
            return Doc::Seq(Vec::new());
        };
        let mut v = Vec::with_capacity(1 + 3 * (t.lead.len() + t.trail.len()));
        for n in &t.lead {
            if n.nl_before {
                v.push(Doc::HardLine);
            }
            v.push(Doc::text(n.text.clone()));
            v.push(if n.line || n.nl_after {
                Doc::HardLine
            } else {
                Doc::Space
            });
        }
        v.push(Doc::text(t.text.clone()));
        for n in &t.trail {
            v.push(if n.nl_before {
                Doc::HardLine
            } else {
                Doc::Space
            });
            v.push(Doc::text(n.text.clone()));
            if n.line || n.nl_after {
                v.push(Doc::HardLine);
            }
        }
        if v.len() == 1 {
            v.pop().unwrap_or(Doc::Seq(Vec::new()))
        } else {
            Doc::Seq(v)
        }
    }

    fn is_unary(&self, p2: Option<usize>, p: usize) -> bool {
        let Some(a) = self.t.get(p) else { return false };
        if a.kind != TokenKind::Operator || !matches!(a.text.as_str(), "-" | "+" | "~" | "!") {
            return false;
        }
        match p2.and_then(|i| self.t.get(i)) {
            None => true,
            Some(x) => match x.kind {
                TokenKind::Operator
                | TokenKind::Comma
                | TokenKind::LParen
                | TokenKind::LBracket => true,
                TokenKind::Keyword => !VALUE_KEYWORDS.contains(&x.upper.as_str()),
                _ => false,
            },
        }
    }

    /// Whether a space separates token `p` (preceded by `p2`) from token `n`.
    fn space(&self, p2: Option<usize>, p: usize, n: usize) -> bool {
        use TokenKind as K;
        let (Some(a), Some(b)) = (self.t.get(p), self.t.get(n)) else {
            return true;
        };
        if !a.trail.is_empty() || !b.lead.is_empty() {
            return true;
        }
        let glued = b.glued;
        match (a.kind, b.kind) {
            (_, K::Comma | K::Semicolon | K::RParen | K::RBracket) => return false,
            (K::LParen | K::LBracket, _) => return false,
            (K::Dot, _) | (_, K::Dot) => return false,
            _ => {}
        }
        if self.is_op(p, "::") || self.is_op(n, "::") {
            return false;
        }
        if a.kind == K::Unknown || b.kind == K::Unknown || self.is_op(p, ":") || self.is_op(n, ":")
        {
            return !glued;
        }
        let brace = |t: &FTok| t.kind == K::Operator && matches!(t.text.as_str(), "{" | "}");
        if brace(a) || brace(b) {
            return !glued;
        }
        if b.kind == K::LParen {
            if a.kind.is_word()
                || matches!(a.kind, K::RParen | K::RBracket | K::Variable | K::Parameter)
            {
                if a.kind == K::Keyword && SPACE_BEFORE_PAREN.contains(&a.upper.as_str()) {
                    return true;
                }
                return !glued;
            }
            return true;
        }
        if b.kind == K::LBracket {
            return !glued;
        }
        if a.kind == K::Operator && b.kind == K::Operator {
            if !glued {
                return true;
            }
            return matches!(b.text.as_str(), "-" | "+")
                && SAFE_BEFORE_UNARY.contains(&a.text.as_str());
        }
        if self.is_unary(p2, p) {
            return false;
        }
        if b.kind == K::String && glued && matches!(a.kind, K::Identifier | K::Keyword) {
            return false; // charset introducer: _utf8'x'
        }
        true
    }

    /// Next index after the unit starting at `i` (a parenthesised group or a CASE expression).
    fn skip(&self, i: usize, to: usize) -> usize {
        let c = self.close.get(i).copied().unwrap_or(usize::MAX);
        if c != usize::MAX && c < to {
            return c + 1;
        }
        let e = self.case_end.get(i).copied().unwrap_or(usize::MAX);
        if e != usize::MAX && e < to {
            return e + 1;
        }
        i + 1
    }

    /// Inline expression for tokens `from..to`.
    fn expr(&self, from: usize, to: usize) -> Doc {
        let mut v = Vec::new();
        let mut prev: Option<usize> = None;
        let mut prev2: Option<usize> = None;
        let mut i = from;
        while i < to {
            if let Some(p) = prev
                && self.space(prev2, p, i)
            {
                v.push(Doc::Space);
            }
            let t = &self.t[i];
            let c = self.close[i];
            let e = self.case_end[i];

            let nest_ok = self.depth.get() < MAX_NEST;
            let last = if nest_ok && t.kind == TokenKind::LParen && c != usize::MAX && c < to {
                self.depth.set(self.depth.get() + 1);
                v.push(self.paren(i, c));
                self.depth.set(self.depth.get().saturating_sub(1));
                c
            } else if nest_ok && t.upper == "CASE" && e != usize::MAX && e < to {
                self.depth.set(self.depth.get() + 1);
                v.push(self.case(i, e));
                self.depth.set(self.depth.get().saturating_sub(1));
                e
            } else {
                v.push(self.tok(i));
                i
            };
            prev2 = prev;
            prev = Some(last);
            i = last + 1;
        }
        Doc::Seq(v)
    }

    /// Depth-0 comma positions in `from..to`.
    fn commas(&self, from: usize, to: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut i = from;
        while i < to {
            if self.t[i].kind == TokenKind::Comma {
                out.push(i);
            }
            i = self.skip(i, to);
        }
        out
    }

    /// Items separated by commas, joined with breakable separators.
    fn list(&self, from: usize, to: usize) -> Doc {
        let commas = self.commas(from, to);
        let mut v = Vec::new();
        let mut start = from;
        for &c in &commas {
            v.push(self.expr(start, c));
            v.push(self.sep(c));
            start = c + 1;
        }
        v.push(self.expr(start, to));
        Doc::Seq(v)
    }

    fn sep(&self, comma: usize) -> Doc {
        if self.opts.comma_first {
            Doc::if_break(
                Doc::Seq(vec![Doc::Line, self.tok(comma), Doc::Space]),
                Doc::Seq(vec![self.tok(comma), Doc::Space]),
            )
        } else {
            Doc::Seq(vec![self.tok(comma), Doc::Line])
        }
    }

    fn paren(&self, open: usize, close: usize) -> Doc {
        if close == open + 1 {
            return Doc::Seq(vec![self.tok(open), self.tok(close)]);
        }
        let mut first = open + 1;
        while first < close && self.t[first].kind == TokenKind::LParen {
            first += 1;
        }
        let inner = if ["SELECT", "WITH", "VALUES"].contains(&self.up(first)) && first == open + 1 {
            self.statement(open + 1, close, false)
        } else {
            self.list(open + 1, close)
        };
        Doc::group(Doc::Seq(vec![
            self.tok(open),
            Doc::indent(Doc::Seq(vec![Doc::SoftLine, inner])),
            Doc::SoftLine,
            self.tok(close),
        ]))
    }

    fn case(&self, from: usize, end: usize) -> Doc {
        // Branch starts (WHEN / ELSE) at depth 0 inside the CASE.
        let mut marks = Vec::new();
        let mut i = from + 1;
        while i < end {
            if matches!(self.up(i), "WHEN" | "ELSE") {
                marks.push(i);
            }
            i = self.skip(i, end);
        }
        let mut v = vec![self.tok(from)];
        let operand_end = marks.first().copied().unwrap_or(end);
        if operand_end > from + 1 {
            v.push(Doc::Space);
            v.push(self.expr(from + 1, operand_end));
        }
        let mut branches = Vec::new();
        for (k, &m) in marks.iter().enumerate() {
            let e = marks.get(k + 1).copied().unwrap_or(end);
            branches.push(Doc::Line);
            branches.push(self.expr(m, e));
        }
        v.push(Doc::indent(Doc::Seq(branches)));
        v.push(Doc::Line);
        v.push(self.tok(end));
        Doc::group(Doc::Seq(v))
    }

    /// A depth-0 AND / OR (not the AND of BETWEEN) in `from..to`.
    fn has_and_or(&self, from: usize, to: usize) -> bool {
        let mut between = false;
        let mut i = from;
        while i < to {
            match self.up(i) {
                "BETWEEN" => between = true,
                "AND" if between => between = false,
                "AND" | "OR" if i > from => return true,
                _ => {}
            }
            i = self.skip(i, to);
        }
        false
    }

    fn cond(&self, from: usize, to: usize) -> Doc {
        let mut v = Vec::new();
        let mut start = from;
        let mut between = false;
        let mut i = from;
        while i < to {
            let u = self.up(i);
            if u == "BETWEEN" {
                between = true;
            } else if u == "AND" && between {
                between = false;
            } else if (u == "AND" || u == "OR") && i > start {
                v.push(self.expr(start, i));
                if self.opts.newline_before_and_or {
                    v.push(Doc::Line);
                    v.push(self.tok(i));
                    v.push(Doc::Space);
                } else {
                    v.push(Doc::Space);
                    v.push(self.tok(i));
                    v.push(Doc::Line);
                }
                start = i + 1;
            }
            i = self.skip(i, to);
        }
        v.push(self.expr(start, to));
        Doc::Seq(v)
    }

    /// The clause keyword starting at `i`: (kind, number of keyword tokens).
    fn clause_kw(
        &self,
        i: usize,
        to: usize,
        at_start: bool,
        head: &str,
        cur: Kind,
    ) -> Option<(Kind, usize)> {
        let w = self.up(i);
        let word = |k: usize| if i + k < to { self.up(i + k) } else { "" };
        let prev = if i > 0 { self.up(i - 1) } else { "" };
        if self.t[i].kind != TokenKind::Keyword && self.t[i].kind != TokenKind::Identifier {
            return None;
        }
        // Part of a qualified name (`t.from`, `order.id`).
        let dot = |j: usize| self.t.get(j).is_some_and(|t| t.kind == TokenKind::Dot);
        if (i > 0 && dot(i - 1)) || dot(i + 1) {
            return None;
        }
        match w {
            "SELECT" => {
                let mut k = 1;
                loop {
                    match word(k) {
                        "DISTINCT"
                        | "ALL"
                        | "DISTINCTROW"
                        | "SQL_CALC_FOUND_ROWS"
                        | "STRAIGHT_JOIN"
                        | "HIGH_PRIORITY" => k += 1,
                        "ON" if word(k - 1) == "DISTINCT"
                            && self
                                .t
                                .get(i + k + 1)
                                .is_some_and(|t| t.kind == TokenKind::LParen) =>
                        {
                            let c = self.close[i + k + 1];
                            if c == usize::MAX || c >= to {
                                break;
                            }
                            k = c + 1 - i;
                        }
                        "TOP" => {
                            k += 1;
                            if self
                                .t
                                .get(i + k)
                                .is_some_and(|t| t.kind == TokenKind::LParen)
                            {
                                let c = self.close[i + k];
                                if c == usize::MAX || c >= to {
                                    break;
                                }
                                k = c + 1 - i;
                            } else if i + k < to {
                                k += 1;
                            }
                            if word(k) == "PERCENT" {
                                k += 1;
                            }
                        }
                        _ => break,
                    }
                }
                Some((Kind::List, k.min(to - i)))
            }
            "FROM" if prev != "DISTINCT" => Some((Kind::List, 1)),
            "WHERE" | "HAVING" | "QUALIFY" | "PREWHERE" => Some((Kind::Cond, 1)),
            "GROUP" | "ORDER" if word(1) == "BY" => Some((Kind::List, 2)),
            "WINDOW" => Some((Kind::List, 1)),
            "LIMIT" | "OFFSET" | "FETCH" => Some((Kind::Inline, 1)),
            "RETURNING" => Some((Kind::List, 1)),
            "UNION" | "INTERSECT" | "EXCEPT" | "MINUS" => Some((
                Kind::SetOp,
                if matches!(word(1), "ALL" | "DISTINCT") {
                    2
                } else {
                    1
                },
            )),
            "INTO" if cur == Kind::List && head == "SELECT" => Some((Kind::Inline, 1)),
            "VALUES" if prev != "DEFAULT" => Some((Kind::List, 1)),
            "SET" if matches!(head, "UPDATE" | "MERGE" | "INSERT") => Some((Kind::List, 1)),
            "ON" if head == "INSERT" && word(1) == "CONFLICT" => Some((Kind::Inline, 2)),
            "ON" if head == "INSERT" && word(1) == "DUPLICATE" => {
                let mut k = 2;
                while k < 4 && matches!(word(k), "KEY" | "UPDATE") {
                    k += 1;
                }
                Some((Kind::List, k))
            }
            "DO" if head == "INSERT" && word(1) == "UPDATE" && word(2) == "SET" => {
                Some((Kind::List, 3))
            }
            "DO" if head == "INSERT" && word(1) == "NOTHING" => Some((Kind::Inline, 2)),
            "INSERT" | "REPLACE" | "UPSERT" if at_start => {
                let mut k = 1;
                while matches!(
                    word(k),
                    "INTO" | "OR" | "IGNORE" | "LOW_PRIORITY" | "DELAYED" | "HIGH_PRIORITY"
                ) && k < 4
                {
                    k += 1;
                }
                Some((Kind::Inline, k))
            }
            "UPDATE" if at_start => Some((Kind::Inline, 1)),
            "DELETE" if at_start => Some((Kind::Inline, if word(1) == "FROM" { 2 } else { 1 })),
            "MERGE" if at_start => Some((Kind::Inline, if word(1) == "INTO" { 2 } else { 1 })),
            "USING" if matches!(head, "DELETE" | "MERGE") => Some((Kind::Inline, 1)),
            "WHEN" if head == "MERGE" => Some((Kind::Inline, 1)),
            "WITH" if at_start => Some((Kind::With, if word(1) == "RECURSIVE" { 2 } else { 1 })),
            "FOR"
                if matches!(
                    word(1),
                    "UPDATE" | "SHARE" | "NO" | "KEY" | "READ" | "XML" | "JSON" | "BROWSE"
                ) =>
            {
                Some((Kind::Inline, 1))
            }
            "LOCK" if word(1) == "IN" => Some((Kind::Inline, 1)),
            _ => {
                // Joins: [NATURAL] [LEFT|RIGHT|FULL] [OUTER] JOIN, CROSS APPLY ...
                let mut k = 0;
                while JOIN_WORDS.contains(&word(k)) && k < 4 {
                    k += 1;
                }
                if matches!(word(k), "JOIN" | "APPLY" | "STRAIGHT_JOIN") {
                    return Some((Kind::Join, k + 1));
                }
                None
            }
        }
    }

    fn clauses(&self, from: usize, to: usize) -> Vec<Clause> {
        let mut out = Vec::new();
        let mut cur = Clause {
            kind: Kind::Inline,
            kw: (from, from),
            body: (from, from),
        };
        let mut head = String::new();
        let mut i = from;
        while i < to {
            let at_start = i == from || (cur.kind == Kind::With);
            if let Some((kind, len)) = self.clause_kw(i, to, at_start, &head, cur.kind) {
                let w = self.up(i).to_string();
                if at_start && QUERY_WORDS.contains(&w.as_str()) {
                    head = if w == "REPLACE" || w == "UPSERT" {
                        "INSERT".into()
                    } else {
                        w
                    };
                } else if head.is_empty() {
                    head = self.up(from).to_string();
                }
                cur.body.1 = i;
                if cur.kw.1 > cur.kw.0 || cur.body.1 > cur.body.0 {
                    out.push(cur);
                }
                let len = len.max(1).min(to - i);
                cur = Clause {
                    kind,
                    kw: (i, i + len),
                    body: (i + len, i + len),
                };
                i += len;
                continue;
            }
            i = self.skip(i, to);
        }
        cur.body.1 = to;
        if cur.kw.1 > cur.kw.0 || cur.body.1 > cur.body.0 {
            out.push(cur);
        }
        out
    }

    fn clause_doc(&self, c: &Clause) -> Doc {
        let force = !self.opts.compact_short_lists;
        let kw = self.expr(c.kw.0, c.kw.1);
        let (bs, be) = c.body;
        let empty = be <= bs;
        match c.kind {
            Kind::Inline => {
                if empty {
                    kw
                } else if c.kw.1 == c.kw.0 {
                    self.expr(bs, be)
                } else {
                    Doc::Seq(vec![kw, Doc::Space, self.expr(bs, be)])
                }
            }
            Kind::List | Kind::Cond => {
                if empty {
                    return kw;
                }
                let single = if c.kind == Kind::List {
                    self.commas(bs, be).is_empty()
                } else {
                    !self.has_and_or(bs, be)
                };
                if single && !force {
                    // One item stays on the keyword line; it breaks internally if needed.
                    return Doc::Seq(vec![kw, Doc::Space, self.expr(bs, be)]);
                }
                let body = if c.kind == Kind::List {
                    self.list(bs, be)
                } else {
                    self.cond(bs, be)
                };
                Doc::group_forced(
                    Doc::Seq(vec![kw, Doc::indent(Doc::Seq(vec![Doc::Line, body]))]),
                    force,
                )
            }
            Kind::SetOp => {
                if empty {
                    kw
                } else {
                    Doc::Seq(vec![kw, Doc::Line, self.expr(bs, be)])
                }
            }
            Kind::Join => {
                if empty {
                    return kw;
                }
                let mut on = None;
                let mut i = bs;
                while i < be {
                    if self.up(i) == "ON" {
                        on = Some(i);
                        break;
                    }
                    i = self.skip(i, be);
                }
                match on {
                    Some(o) if o + 1 < be => Doc::group(Doc::Seq(vec![
                        kw,
                        Doc::Space,
                        self.expr(bs, o),
                        Doc::indent(Doc::Seq(vec![
                            Doc::Line,
                            self.tok(o),
                            Doc::Space,
                            self.cond(o + 1, be),
                        ])),
                    ])),
                    _ => Doc::Seq(vec![kw, Doc::Space, self.expr(bs, be)]),
                }
            }
            Kind::With => {
                if empty {
                    return kw;
                }
                let commas = self.commas(bs, be);
                let mut v = vec![kw, Doc::Space];
                let mut start = bs;
                for &cm in &commas {
                    v.push(self.expr(start, cm));
                    v.push(self.tok(cm));
                    v.push(Doc::HardLine);
                    start = cm + 1;
                }
                v.push(self.expr(start, be));
                Doc::Seq(v)
            }
        }
    }

    /// A query or DML statement: clauses joined by line breaks.
    fn statement(&self, from: usize, to: usize, top: bool) -> Doc {
        let clauses = self.clauses(from, to);
        let mut v = Vec::with_capacity(clauses.len() * 2);
        for (k, c) in clauses.iter().enumerate() {
            if k > 0 {
                v.push(Doc::Line);
            }
            v.push(self.clause_doc(c));
        }
        Doc::group_forced(Doc::Seq(v), top)
    }

    /// Items separated by top-level commas, breaking after the commas when too long.
    fn generic(&self, from: usize, to: usize) -> Doc {
        let commas = self.commas(from, to);
        if commas.is_empty() {
            return self.expr(from, to);
        }
        let mut rest = Vec::new();
        let mut start = commas[0] + 1;
        rest.push(self.sep(commas[0]));
        for &c in &commas[1..] {
            rest.push(self.expr(start, c));
            rest.push(self.sep(c));
            start = c + 1;
        }
        rest.push(self.expr(start, to));
        Doc::group(Doc::Seq(vec![
            self.expr(from, commas[0]),
            Doc::indent(Doc::Seq(rest)),
        ]))
    }

    fn find_top(&self, from: usize, to: usize, pred: impl Fn(usize) -> bool) -> Option<usize> {
        let mut i = from;
        while i < to {
            if pred(i) {
                return Some(i);
            }
            i = self.skip(i, to);
        }
        None
    }

    /// Layout of a whole statement (all tokens).
    pub(crate) fn layout(&self) -> Doc {
        let n = self.t.len();
        if n == 0 {
            return Doc::Seq(Vec::new());
        }
        let mut first = 0;
        while first < n && self.t[first].kind == TokenKind::LParen {
            first += 1;
        }
        let head = self.up(first);
        if first > 0 || QUERY_WORDS.contains(&head) {
            return self.statement(0, n, true);
        }
        match head {
            "EXPLAIN" | "DESCRIBE" | "DESC" => {
                if let Some(q) = self.find_top(1, n, |i| {
                    [
                        "SELECT", "WITH", "INSERT", "UPDATE", "DELETE", "VALUES", "MERGE",
                    ]
                    .contains(&self.up(i))
                }) {
                    return Doc::Seq(vec![
                        self.expr(0, q),
                        Doc::Space,
                        self.statement(q, n, true),
                    ]);
                }
                self.generic(0, n)
            }
            "CREATE" => self.create(n),
            _ => self.generic(0, n),
        }
    }

    fn create(&self, n: usize) -> Doc {
        let kind = (1..n.min(8)).find(|&i| matches!(self.up(i), "TABLE" | "VIEW"));
        let Some(k) = kind else {
            return self.generic(0, n);
        };
        if self.up(k) == "VIEW" {
            if let Some(a) = self.find_top(k + 1, n, |i| self.up(i) == "AS")
                && a + 1 < n
            {
                return Doc::Seq(vec![
                    self.expr(0, a + 1),
                    Doc::HardLine,
                    self.statement(a + 1, n, true),
                ]);
            }
            return self.generic(0, n);
        }
        // CREATE TABLE name ( definitions ) [options] | CREATE TABLE name AS query
        let open = self.find_top(k + 1, n, |i| {
            self.t[i].kind == TokenKind::LParen || self.up(i) == "AS"
        });
        match open {
            Some(a) if self.up(a) == "AS" && a + 1 < n => Doc::Seq(vec![
                self.expr(0, a + 1),
                Doc::HardLine,
                self.statement(a + 1, n, true),
            ]),
            Some(o) if self.t[o].kind == TokenKind::LParen && self.close[o] != usize::MAX => {
                let c = self.close[o];
                let commas = self.commas(o + 1, c);
                let mut defs = vec![Doc::HardLine];
                let mut start = o + 1;
                for &cm in &commas {
                    defs.push(self.expr(start, cm));
                    defs.push(self.tok(cm));
                    defs.push(Doc::HardLine);
                    start = cm + 1;
                }
                defs.push(self.expr(start, c));
                let mut v = vec![
                    self.expr(0, o),
                    Doc::Space,
                    self.tok(o),
                    Doc::indent(Doc::Seq(defs)),
                    Doc::HardLine,
                    self.tok(c),
                ];
                if c + 1 < n {
                    v.push(Doc::Space);
                    if self.up(c + 1) == "AS" && c + 2 < n {
                        v.push(self.tok(c + 1));
                        v.push(Doc::HardLine);
                        v.push(self.statement(c + 2, n, true));
                    } else {
                        v.push(self.generic(c + 1, n));
                    }
                }
                Doc::Seq(v)
            }
            _ => self.generic(0, n),
        }
    }
}
