//! A small lexer for DuckDB's PostgreSQL style SQL: statement splitting,
//! statement classification and a few helpers for parsing catalog SQL.
//!
//! Rules: `'...'` strings (`''` escape), `E'...'` strings with backslash
//! escapes, `$tag$ ... $tag$` dollar quoting, `"..."` identifiers, `--` and
//! nested `/* */` comments, `?`, `$1` and `$name` parameters.

/// Kind of a lexical token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokKind {
    Space,
    Comment,
    Word,
    QuotedIdent,
    String,
    Number,
    Variable,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Semi,
    Dot,
    Operator,
    /// Unterminated string, identifier or comment.
    Illegal,
}

/// One token with its byte offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Token<'a> {
    pub kind: TokKind,
    pub text: &'a str,
    pub start: usize,
}

impl Token<'_> {
    pub fn end(&self) -> usize {
        self.start + self.text.len()
    }

    pub fn is_trivia(&self) -> bool {
        matches!(self.kind, TokKind::Space | TokKind::Comment)
    }

    pub fn is_word(&self, kw: &str) -> bool {
        self.kind == TokKind::Word && self.text.eq_ignore_ascii_case(kw)
    }

    pub fn is_any_word(&self, kws: &[&str]) -> bool {
        self.kind == TokKind::Word && kws.iter().any(|k| self.text.eq_ignore_ascii_case(k))
    }

    /// Identifier value of a word or quoted identifier.
    pub fn ident(&self) -> Option<String> {
        match self.kind {
            TokKind::Word => Some(self.text.to_string()),
            TokKind::QuotedIdent => {
                let inner = self.text.strip_prefix('"').unwrap_or(self.text);
                let inner = inner.strip_suffix('"').unwrap_or(inner);
                Some(inner.replace("\"\"", "\""))
            }
            _ => None,
        }
    }
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c >= 0x80
}

fn is_ident_char(c: u8) -> bool {
    is_ident_start(c) || c.is_ascii_digit() || c == b'$'
}

/// Scan `'...'` (or `"..."`) with doubled-quote escapes; `backslash` also
/// honours `\` escapes (E strings). Returns false when unterminated.
fn scan_quoted(b: &[u8], i: &mut usize, quote: u8, backslash: bool) -> bool {
    *i += 1;
    while *i < b.len() {
        let c = b[*i];
        if backslash && c == b'\\' {
            *i += 2;
            continue;
        }
        if c == quote {
            if b.get(*i + 1) == Some(&quote) {
                *i += 2;
                continue;
            }
            *i += 1;
            return true;
        }
        *i += 1;
    }
    *i = b.len();
    false
}

/// At `$`: length of a dollar-quote opening tag (`$$` or `$tag$`).
fn dollar_tag_len(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    if b.get(j).is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    while j < b.len() && (is_ident_start(b[j]) || b[j].is_ascii_digit()) {
        j += 1;
    }
    (b.get(j) == Some(&b'$')).then_some(j + 1 - i)
}

/// Tokenize `sql` completely (tokens cover the input).
pub(crate) fn tokenize(sql: &str) -> Vec<Token<'_>> {
    let b = sql.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 3 + 1);
    let mut i = 0usize;
    while i < b.len() {
        let start = i;
        let c = b[i];
        let next = b.get(i + 1).copied();
        let kind = match c {
            b' ' | b'\t' | b'\n' | b'\r' | 0x0c => {
                while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r' | 0x0c) {
                    i += 1;
                }
                TokKind::Space
            }
            b'-' if next == Some(b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                TokKind::Comment
            }
            b'/' if next == Some(b'*') => {
                let mut depth = 0usize;
                let mut closed = false;
                while i < b.len() {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            closed = true;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                if closed {
                    TokKind::Comment
                } else {
                    TokKind::Illegal
                }
            }
            b'\'' => {
                if scan_quoted(b, &mut i, b'\'', false) {
                    TokKind::String
                } else {
                    TokKind::Illegal
                }
            }
            b'e' | b'E' if next == Some(b'\'') => {
                i += 1;
                if scan_quoted(b, &mut i, b'\'', true) {
                    TokKind::String
                } else {
                    TokKind::Illegal
                }
            }
            b'"' => {
                if scan_quoted(b, &mut i, b'"', false) {
                    TokKind::QuotedIdent
                } else {
                    TokKind::Illegal
                }
            }
            b'$' => match dollar_tag_len(b, i) {
                Some(n) => {
                    let tag = &sql[i..i + n];
                    match sql[i + n..].find(tag) {
                        Some(p) => {
                            i += n + p + n;
                            TokKind::String
                        }
                        None => {
                            i = b.len();
                            TokKind::Illegal
                        }
                    }
                }
                None => {
                    i += 1;
                    while i < b.len() && (is_ident_start(b[i]) || b[i].is_ascii_digit()) {
                        i += 1;
                    }
                    if i == start + 1 {
                        TokKind::Operator
                    } else {
                        TokKind::Variable
                    }
                }
            },
            b'?' => {
                i += 1;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                TokKind::Variable
            }
            b'0'..=b'9' => {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'.')
                {
                    if matches!(b[i], b'e' | b'E') && matches!(b.get(i + 1), Some(b'+' | b'-')) {
                        i += 1;
                    }
                    i += 1;
                }
                TokKind::Number
            }
            b'.' if next.is_some_and(|n| n.is_ascii_digit()) => {
                i += 1;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                TokKind::Number
            }
            c if is_ident_start(c) => {
                while i < b.len() && is_ident_char(b[i]) {
                    i += 1;
                }
                TokKind::Word
            }
            b'(' => {
                i += 1;
                TokKind::LParen
            }
            b')' => {
                i += 1;
                TokKind::RParen
            }
            b'[' => {
                i += 1;
                TokKind::LBracket
            }
            b']' => {
                i += 1;
                TokKind::RBracket
            }
            b',' => {
                i += 1;
                TokKind::Comma
            }
            b';' => {
                i += 1;
                TokKind::Semi
            }
            b'.' => {
                i += 1;
                TokKind::Dot
            }
            _ => {
                i += 1;
                TokKind::Operator
            }
        };
        out.push(Token {
            kind,
            text: &sql[start..i],
            start,
        });
    }
    out
}

/// Tokens without whitespace and comments.
pub(crate) fn significant(sql: &str) -> Vec<Token<'_>> {
    tokenize(sql)
        .into_iter()
        .filter(|t| !t.is_trivia())
        .collect()
}

/// One statement of a script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StatementSpan<'a> {
    /// Byte offset of the statement inside the script.
    pub offset: usize,
    /// Statement text (first significant token through the `;`, if any).
    pub text: &'a str,
}

/// Split a script at top level semicolons; empty statements are dropped.
pub(crate) fn split_statements(sql: &str) -> Vec<StatementSpan<'_>> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for t in tokenize(sql) {
        if t.is_trivia() {
            continue;
        }
        if t.kind == TokKind::Semi {
            if let Some(s) = start.take() {
                out.push(StatementSpan {
                    offset: s,
                    text: &sql[s..t.end()],
                });
            }
            continue;
        }
        if start.is_none() {
            start = Some(t.start);
        }
    }
    if let Some(s) = start {
        out.push(StatementSpan {
            offset: s,
            text: sql[s..].trim_end(),
        });
    }
    out
}

/// What a statement does, as far as the driver needs to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StatementKind {
    Query,
    Insert,
    Update,
    Delete,
    Merge,
    Begin,
    Commit,
    Rollback,
    /// Statements that must not be wrapped into an implicit transaction
    /// (PRAGMA, SET, ATTACH, CHECKPOINT, VACUUM, INSTALL, LOAD, USE, EXPLAIN ...).
    Session(String),
    /// CREATE / DROP / ALTER / COMMENT with the command tag.
    Ddl(String),
    Other(String),
}

impl StatementKind {
    pub fn is_dml(&self) -> bool {
        matches!(
            self,
            StatementKind::Insert
                | StatementKind::Update
                | StatementKind::Delete
                | StatementKind::Merge
        )
    }

    pub fn is_transaction_control(&self) -> bool {
        matches!(
            self,
            StatementKind::Begin | StatementKind::Commit | StatementKind::Rollback
        )
    }

    pub fn runs_outside_transaction(&self) -> bool {
        self.is_transaction_control() || matches!(self, StatementKind::Session(_))
    }

    pub fn tag(&self) -> String {
        match self {
            StatementKind::Query => "SELECT".into(),
            StatementKind::Insert => "INSERT".into(),
            StatementKind::Update => "UPDATE".into(),
            StatementKind::Delete => "DELETE".into(),
            StatementKind::Merge => "MERGE".into(),
            StatementKind::Begin => "BEGIN".into(),
            StatementKind::Commit => "COMMIT".into(),
            StatementKind::Rollback => "ROLLBACK".into(),
            StatementKind::Session(t) | StatementKind::Ddl(t) | StatementKind::Other(t) => {
                t.clone()
            }
        }
    }
}

fn with_main_verb(toks: &[Token<'_>]) -> StatementKind {
    let mut depth = 0i32;
    for t in toks.iter().skip(1) {
        match t.kind {
            TokKind::LParen => depth += 1,
            TokKind::RParen => depth -= 1,
            TokKind::Word if depth == 0 => {
                if t.is_any_word(&["SELECT", "VALUES", "FROM", "TABLE"]) {
                    return StatementKind::Query;
                } else if t.is_word("INSERT") {
                    return StatementKind::Insert;
                } else if t.is_word("UPDATE") {
                    return StatementKind::Update;
                } else if t.is_word("DELETE") {
                    return StatementKind::Delete;
                } else if t.is_word("MERGE") {
                    return StatementKind::Merge;
                }
            }
            _ => {}
        }
    }
    StatementKind::Query
}

/// Classify a statement by its leading keywords.
pub(crate) fn classify(stmt: &str) -> StatementKind {
    let toks = significant(stmt);
    let Some(first) = toks.first() else {
        return StatementKind::Other(String::new());
    };
    if first.kind == TokKind::LParen {
        return StatementKind::Query;
    }
    if first.kind != TokKind::Word {
        return StatementKind::Other(String::new());
    }
    let word = first.text.to_ascii_uppercase();
    match word.as_str() {
        "SELECT" | "VALUES" | "FROM" | "TABLE" | "SHOW" | "DESCRIBE" | "DESC" | "SUMMARIZE"
        | "PIVOT" | "UNPIVOT" => StatementKind::Query,
        "WITH" => with_main_verb(&toks),
        "INSERT" => StatementKind::Insert,
        "UPDATE" => StatementKind::Update,
        "DELETE" => StatementKind::Delete,
        "MERGE" => StatementKind::Merge,
        "BEGIN" | "START" => StatementKind::Begin,
        "COMMIT" | "END" => StatementKind::Commit,
        "ROLLBACK" | "ABORT" => StatementKind::Rollback,
        "PRAGMA" | "SET" | "RESET" | "ATTACH" | "DETACH" | "USE" | "CHECKPOINT" | "FORCE"
        | "VACUUM" | "INSTALL" | "LOAD" | "EXPLAIN" => StatementKind::Session(word),
        "CREATE" | "DROP" | "ALTER" | "COMMENT" => {
            let object = toks
                .iter()
                .skip(1)
                .find(|t| {
                    t.is_any_word(&[
                        "TABLE", "VIEW", "INDEX", "SEQUENCE", "SCHEMA", "MACRO", "FUNCTION",
                        "TYPE", "SECRET", "DATABASE",
                    ])
                })
                .map(|t| t.text.to_ascii_uppercase());
            match object {
                Some(o) => StatementKind::Ddl(format!("{word} {o}")),
                None => StatementKind::Ddl(word),
            }
        }
        _ => StatementKind::Other(word),
    }
}

/// `EXPLAIN ANALYZE ...` / `EXPLAIN (ANALYZE ...) ...`: runs the statement.
pub(crate) fn is_explain_analyze(stmt: &str) -> bool {
    let toks = significant(stmt);
    if !toks.first().is_some_and(|t| t.is_word("EXPLAIN")) {
        return false;
    }
    match toks.get(1) {
        Some(t) if t.is_word("ANALYZE") => true,
        Some(t) if t.kind == TokKind::LParen => toks
            .iter()
            .skip(2)
            .take_while(|t| t.kind != TokKind::RParen)
            .any(|t| t.is_word("ANALYZE")),
        _ => false,
    }
}

/// The statement has a `RETURNING` clause (at nesting level 0).
pub(crate) fn has_returning(stmt: &str) -> bool {
    let mut depth = 0i32;
    for t in significant(stmt) {
        match t.kind {
            TokKind::LParen => depth += 1,
            TokKind::RParen => depth -= 1,
            TokKind::Word if depth == 0 && t.is_word("RETURNING") => return true,
            _ => {}
        }
    }
    false
}

/// Base table of a simple single-table query: `SELECT ... FROM [db.][schema.]table [alias]`
/// without joins, set operations, grouping or subqueries in FROM.
/// Returns (database, schema, table).
pub(crate) fn single_table_source(stmt: &str) -> Option<(Option<String>, Option<String>, String)> {
    let toks = significant(stmt);
    let first = toks.first()?;
    if !first.is_word("SELECT") {
        return None;
    }
    let mut depth = 0i32;
    let mut from = None;
    for (i, t) in toks.iter().enumerate() {
        match t.kind {
            TokKind::LParen => depth += 1,
            TokKind::RParen => depth -= 1,
            TokKind::Word if depth == 0 => {
                if t.is_any_word(&[
                    "UNION",
                    "INTERSECT",
                    "EXCEPT",
                    "GROUP",
                    "JOIN",
                    "DISTINCT",
                    "PIVOT",
                    "UNPIVOT",
                    "QUALIFY",
                ]) {
                    return None;
                }
                if t.is_word("FROM") && from.is_none() {
                    from = Some(i + 1);
                }
            }
            _ => {}
        }
    }
    let mut i = from?;
    let mut parts = Vec::new();
    loop {
        let t = toks.get(i)?;
        parts.push(t.ident()?);
        i += 1;
        if toks.get(i).is_some_and(|t| t.kind == TokKind::Dot) {
            i += 1;
        } else {
            break;
        }
    }
    // after the table: nothing, an alias, WHERE / ORDER / LIMIT ...; never a comma (implicit join)
    let mut j = i;
    while let Some(t) = toks.get(j) {
        if t.kind == TokKind::Comma && depth_at(&toks, j) == 0 {
            return None;
        }
        if t.is_any_word(&["WHERE", "ORDER", "LIMIT", "OFFSET", "WINDOW"]) {
            break;
        }
        j += 1;
    }
    match parts.len() {
        1 => Some((None, None, parts.remove(0))),
        2 => {
            let table = parts.remove(1);
            Some((None, Some(parts.remove(0)), table))
        }
        3 => {
            let table = parts.remove(2);
            let schema = parts.remove(1);
            Some((Some(parts.remove(0)), Some(schema), table))
        }
        _ => None,
    }
}

fn depth_at(toks: &[Token<'_>], idx: usize) -> i32 {
    let mut depth = 0;
    for t in &toks[..idx] {
        match t.kind {
            TokKind::LParen => depth += 1,
            TokKind::RParen => depth -= 1,
            _ => {}
        }
    }
    depth
}

/// Select list items of a simple query: each is `*`, or `column [[AS] alias]`
/// mapped to (output name, source column). `None` when an item is not a plain column.
pub(crate) fn select_items(stmt: &str) -> Option<Vec<SelectItem>> {
    let toks = significant(stmt);
    if !toks.first()?.is_word("SELECT") {
        return None;
    }
    let mut items = Vec::new();
    let mut current: Vec<Token<'_>> = Vec::new();
    let mut depth = 0i32;
    for t in toks.iter().skip(1) {
        match t.kind {
            TokKind::LParen => depth += 1,
            TokKind::RParen => depth -= 1,
            _ => {}
        }
        if depth == 0 && t.is_word("FROM") {
            break;
        }
        if depth == 0 && t.kind == TokKind::Comma {
            items.push(select_item(&current));
            current.clear();
        } else {
            current.push(*t);
        }
    }
    if !current.is_empty() {
        items.push(select_item(&current));
    }
    Some(items)
}

/// One item of a select list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SelectItem {
    Star,
    /// Output name and source column.
    Column(String, String),
    Expression,
}

fn select_item(toks: &[Token<'_>]) -> SelectItem {
    let t: Vec<&Token<'_>> = toks.iter().collect();
    match t.as_slice() {
        [s] if s.text == "*" => SelectItem::Star,
        [q, d, s] if d.kind == TokKind::Dot && s.text == "*" && q.ident().is_some() => {
            SelectItem::Star
        }
        _ => {
            // column [AS alias] | qualifier.column [AS alias]
            let (col_end, col) = if t.len() >= 3 && t[1].kind == TokKind::Dot {
                (3, t[2].ident())
            } else {
                (1, t.first().and_then(|x| x.ident()))
            };
            let Some(col) = col else {
                return SelectItem::Expression;
            };
            if t.first()
                .is_some_and(|x| x.kind == TokKind::Word && is_reserved_start(x))
            {
                return SelectItem::Expression;
            }
            match &t[col_end.min(t.len())..] {
                [] => SelectItem::Column(col.clone(), col),
                [a] if a.ident().is_some() => {
                    SelectItem::Column(a.ident().unwrap_or_default(), col)
                }
                [kw, a] if kw.is_word("AS") && a.ident().is_some() => {
                    SelectItem::Column(a.ident().unwrap_or_default(), col)
                }
                _ => SelectItem::Expression,
            }
        }
    }
}

fn is_reserved_start(t: &Token<'_>) -> bool {
    t.is_any_word(&[
        "CASE",
        "CAST",
        "NOT",
        "NULL",
        "TRUE",
        "FALSE",
        "DISTINCT",
        "EXISTS",
        "INTERVAL",
        "DATE",
        "TIMESTAMP",
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_cover_input() {
        let sql = "SELECT 'it''s', E'a\\'b', $$x;y$$, $tag$ q $tag$, \"Q\"\"x\", ?, $1, $name, 1.5e-3, [1,2] -- c\n/* a /* nested */ b */;";
        let toks = tokenize(sql);
        assert_eq!(toks.iter().map(|t| t.text).collect::<String>(), sql);
        assert!(toks.iter().all(|t| t.kind != TokKind::Illegal), "{toks:?}");
        let vars: Vec<&str> = toks
            .iter()
            .filter(|t| t.kind == TokKind::Variable)
            .map(|t| t.text)
            .collect();
        assert_eq!(vars, vec!["?", "$1", "$name"]);
    }

    #[test]
    fn splitting() {
        let s =
            split_statements("CREATE MACRO m(a) AS a + 1; SELECT ';' ; SELECT $$;$$;\n-- end\n");
        let texts: Vec<&str> = s.iter().map(|s| s.text).collect();
        assert_eq!(
            texts,
            vec![
                "CREATE MACRO m(a) AS a + 1;",
                "SELECT ';' ;",
                "SELECT $$;$$;"
            ]
        );
        assert!(split_statements(" ; -- x\n").is_empty());
    }

    #[test]
    fn classification() {
        assert_eq!(classify("FROM t"), StatementKind::Query);
        assert_eq!(
            classify("with x as (select 1) insert into t select * from x"),
            StatementKind::Insert
        );
        assert_eq!(classify("START TRANSACTION"), StatementKind::Begin);
        assert!(classify("CHECKPOINT").runs_outside_transaction());
        assert_eq!(
            classify("create or replace table t(a int)").tag(),
            "CREATE TABLE"
        );
        assert!(classify("DELETE FROM t").is_dml());
        assert!(has_returning("INSERT INTO t VALUES (1) RETURNING *"));
        assert!(!has_returning("INSERT INTO t SELECT (SELECT 1 RETURNING) "));
        assert!(is_explain_analyze("explain analyze select 1"));
        assert!(is_explain_analyze(
            "EXPLAIN (FORMAT json, ANALYZE) DELETE FROM t"
        ));
        assert!(!is_explain_analyze(
            "EXPLAIN (FORMAT json) SELECT analyze FROM t"
        ));
        assert!(!is_explain_analyze("SELECT 1"));
    }

    #[test]
    fn single_table_queries() {
        assert_eq!(
            single_table_source("SELECT * FROM t WHERE a = 1"),
            Some((None, None, "t".into()))
        );
        assert_eq!(
            single_table_source("select a from main.\"T x\" x"),
            Some((None, Some("main".into()), "T x".into()))
        );
        assert_eq!(
            single_table_source("SELECT * FROM db.s.t"),
            Some((Some("db".into()), Some("s".into()), "t".into()))
        );
        assert_eq!(single_table_source("SELECT * FROM a JOIN b ON true"), None);
        assert_eq!(single_table_source("SELECT * FROM a, b"), None);
        assert_eq!(
            single_table_source("SELECT count(*) FROM a GROUP BY b"),
            None
        );
        assert_eq!(single_table_source("SELECT * FROM (SELECT 1)"), None);
        let items = select_items("SELECT a, t.b AS bb, c cc, *, a + 1, count(*) FROM t").unwrap();
        assert_eq!(
            items,
            vec![
                SelectItem::Column("a".into(), "a".into()),
                SelectItem::Column("bb".into(), "b".into()),
                SelectItem::Column("cc".into(), "c".into()),
                SelectItem::Star,
                SelectItem::Expression,
                SelectItem::Expression,
            ]
        );
    }
}
