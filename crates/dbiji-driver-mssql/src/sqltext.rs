//! Small T-SQL lexer and text utilities: statement classification, `GO`
//! batch splitting, scope analysis for `sp_executesql`, module header
//! parsing and query shaping helpers.

/// Token category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokKind {
    /// Keyword or unquoted identifier (also `#temp` names).
    Word,
    /// `[ident]` or `"ident"`.
    Quoted,
    /// `'...'` or `N'...'`.
    Str,
    /// Numeric or `0x..` binary literal.
    Number,
    /// `@var` / `@@function`.
    Variable,
    /// Any other single character.
    Punct,
}

/// One token with its byte offset and parenthesis depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tok<'a> {
    /// Category.
    pub kind: TokKind,
    /// Source text.
    pub text: &'a str,
    /// Byte offset in the source.
    pub start: usize,
    /// Parenthesis nesting depth at the token (0 = top level).
    pub depth: i32,
}

impl Tok<'_> {
    /// Case-insensitive keyword comparison.
    pub fn is(&self, word: &str) -> bool {
        self.kind == TokKind::Word && self.text.eq_ignore_ascii_case(word)
    }

    /// Byte offset just after the token.
    pub fn end(&self) -> usize {
        self.start + self.text.len()
    }
}

fn is_word_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '#'
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '#' || c == '$' || c == '@'
}

/// Tokenize T-SQL, skipping whitespace and comments (block comments nest).
pub fn tokenize(sql: &str) -> Vec<Tok<'_>> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut depth = 0i32;
    while i < sql.len() {
        let c = match sql[i..].chars().next() {
            Some(c) => c,
            None => break,
        };
        let clen = c.len_utf8();
        if c.is_whitespace() {
            i += clen;
            continue;
        }
        // line comment
        if sql[i..].starts_with("--") {
            i = sql[i..].find('\n').map(|n| i + n + 1).unwrap_or(sql.len());
            continue;
        }
        // nested block comment
        if sql[i..].starts_with("/*") {
            let mut level = 0;
            let mut j = i;
            while j < sql.len() {
                if sql[j..].starts_with("/*") {
                    level += 1;
                    j += 2;
                } else if sql[j..].starts_with("*/") {
                    level -= 1;
                    j += 2;
                    if level == 0 {
                        break;
                    }
                } else {
                    j += sql[j..].chars().next().map(char::len_utf8).unwrap_or(1);
                }
            }
            i = j.min(sql.len());
            continue;
        }
        let start = i;
        // N'...' national string
        let string_start = if c == '\'' {
            Some(i)
        } else if (c == 'N' || c == 'n') && bytes.get(i + 1) == Some(&b'\'') {
            Some(i + 1)
        } else {
            None
        };
        if let Some(q) = string_start {
            let mut j = q + 1;
            loop {
                match sql[j..].find('\'') {
                    Some(n) => {
                        j += n + 1;
                        if bytes.get(j) == Some(&b'\'') {
                            j += 1;
                            continue;
                        }
                        break;
                    }
                    None => {
                        j = sql.len();
                        break;
                    }
                }
            }
            out.push(Tok { kind: TokKind::Str, text: &sql[start..j], start, depth });
            i = j;
            continue;
        }
        if c == '[' || c == '"' {
            let close = if c == '[' { ']' } else { '"' };
            let mut j = i + 1;
            loop {
                match sql[j..].find(close) {
                    Some(n) => {
                        j += n + 1;
                        if sql[j..].starts_with(close) {
                            j += 1;
                            continue;
                        }
                        break;
                    }
                    None => {
                        j = sql.len();
                        break;
                    }
                }
            }
            out.push(Tok { kind: TokKind::Quoted, text: &sql[start..j], start, depth });
            i = j;
            continue;
        }
        if c == '@' {
            let mut j = i + 1;
            while j < sql.len() {
                let d = match sql[j..].chars().next() {
                    Some(d) => d,
                    None => break,
                };
                if is_word_char(d) {
                    j += d.len_utf8();
                } else {
                    break;
                }
            }
            out.push(Tok { kind: TokKind::Variable, text: &sql[start..j], start, depth });
            i = j;
            continue;
        }
        if c.is_ascii_digit() || (c == '.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            let mut j = i + 1;
            while j < sql.len() {
                let d = bytes[j] as char;
                let exponent_sign = (d == '+' || d == '-') && matches!(bytes[j - 1], b'e' | b'E') && !sql[start..].starts_with("0x");
                if d.is_ascii_alphanumeric() || d == '.' || exponent_sign {
                    j += 1;
                } else {
                    break;
                }
            }
            out.push(Tok { kind: TokKind::Number, text: &sql[start..j], start, depth });
            i = j;
            continue;
        }
        if is_word_start(c) {
            let mut j = i + clen;
            while j < sql.len() {
                let d = match sql[j..].chars().next() {
                    Some(d) => d,
                    None => break,
                };
                if is_word_char(d) {
                    j += d.len_utf8();
                } else {
                    break;
                }
            }
            out.push(Tok { kind: TokKind::Word, text: &sql[start..j], start, depth });
            i = j;
            continue;
        }
        if c == ')' {
            depth -= 1;
        }
        out.push(Tok { kind: TokKind::Punct, text: &sql[start..start + clen], start, depth });
        if c == '(' {
            depth += 1;
        }
        i += clen;
    }
    out
}

/// First keyword of the statement, upper case.
pub fn first_keyword(sql: &str) -> Option<String> {
    tokenize(sql).into_iter().find(|t| t.kind == TokKind::Word).map(|t| t.text.to_ascii_uppercase())
}

/// Broad statement category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StmtClass {
    /// Query returning rows.
    Select,
    /// INSERT / UPDATE / DELETE / MERGE.
    Dml,
    /// Anything else (DDL, EXEC, control flow, SET ...).
    Other,
}

/// Classify a statement (CTEs are looked through).
pub fn classify(sql: &str) -> StmtClass {
    let toks = tokenize(sql);
    let Some(first) = toks.iter().find(|t| t.kind != TokKind::Punct || t.text != ";") else {
        return StmtClass::Other;
    };
    let class_of = |t: &Tok<'_>| -> Option<StmtClass> {
        if t.is("SELECT") {
            Some(StmtClass::Select)
        } else if t.is("INSERT") || t.is("UPDATE") || t.is("DELETE") || t.is("MERGE") {
            Some(StmtClass::Dml)
        } else {
            None
        }
    };
    if first.is("WITH") {
        // first top-level statement keyword after the CTE definitions
        return toks.iter().filter(|t| t.depth == 0 && t.start > first.start).find_map(class_of).unwrap_or(StmtClass::Other);
    }
    if first.kind == TokKind::Punct && first.text == "(" {
        return StmtClass::Select;
    }
    class_of(first).unwrap_or(StmtClass::Other)
}

/// Session options whose `SET` only lasts for the current module scope.
const SESSION_OPTIONS: &[&str] = &[
    "ANSI_DEFAULTS",
    "ANSI_NULL_DFLT_OFF",
    "ANSI_NULL_DFLT_ON",
    "ANSI_NULLS",
    "ANSI_PADDING",
    "ANSI_WARNINGS",
    "ARITHABORT",
    "ARITHIGNORE",
    "CONCAT_NULL_YIELDS_NULL",
    "CONTEXT_INFO",
    "CURSOR_CLOSE_ON_COMMIT",
    "DATEFIRST",
    "DATEFORMAT",
    "DEADLOCK_PRIORITY",
    "FIPS_FLAGGER",
    "FMTONLY",
    "FORCEPLAN",
    "IDENTITY_INSERT",
    "IMPLICIT_TRANSACTIONS",
    "LANGUAGE",
    "LOCK_TIMEOUT",
    "NOCOUNT",
    "NOEXEC",
    "NUMERIC_ROUNDABORT",
    "OFFSETS",
    "PARSEONLY",
    "QUERY_GOVERNOR_COST_LIMIT",
    "QUOTED_IDENTIFIER",
    "REMOTE_PROC_TRANSACTIONS",
    "ROWCOUNT",
    "SHOWPLAN_ALL",
    "SHOWPLAN_TEXT",
    "SHOWPLAN_XML",
    "STATISTICS",
    "TEXTSIZE",
    "TRANSACTION",
    "XACT_ABORT",
    "NO_BROWSETABLE",
];

/// Does the batch contain statements whose effect would be lost (or that
/// would fail) when executed inside `sp_executesql`? (`USE`, session `SET`
/// options, transaction control, temporary table creation, `EXECUTE AS`.)
pub fn is_scope_sensitive(sql: &str) -> bool {
    let toks = tokenize(sql);
    for (i, t) in toks.iter().enumerate() {
        if t.kind != TokKind::Word {
            continue;
        }
        let next = toks.get(i + 1);
        let prev = i.checked_sub(1).and_then(|p| toks.get(p));
        let upper = t.text.to_ascii_uppercase();
        match upper.as_str() {
            "USE" | "REVERT" | "SETUSER" => return true,
            "SET" => {
                if let Some(n) = next
                    && n.kind == TokKind::Word
                    && SESSION_OPTIONS.iter().any(|o| n.text.eq_ignore_ascii_case(o))
                {
                    return true;
                }
            }
            "BEGIN" => {
                if next.is_some_and(|n| n.is("TRAN") || n.is("TRANSACTION") || n.is("DISTRIBUTED")) {
                    return true;
                }
            }
            "COMMIT" | "ROLLBACK" => return true,
            "SAVE" => {
                if next.is_some_and(|n| n.is("TRAN") || n.is("TRANSACTION")) {
                    return true;
                }
            }
            "EXECUTE" | "EXEC" => {
                if next.is_some_and(|n| n.is("AS")) {
                    return true;
                }
            }
            "OPEN" => {
                if next.is_some_and(|n| n.is("SYMMETRIC") || n.is("MASTER")) {
                    return true;
                }
            }
            "TABLE" | "INTO" => {
                let temp = next.is_some_and(|n| n.kind == TokKind::Word && n.text.starts_with('#'));
                let inserting = prev.is_some_and(|p| p.is("INSERT") || p.is("MERGE") || p.is("ALTER") || p.is("TRUNCATE") || p.is("DROP"));
                if temp && !inserting {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Does the SQL mention transaction control or `USE` (session state the
/// driver tracks)?
pub fn touches_session_state(sql: &str) -> bool {
    tokenize(sql).iter().any(|t| {
        t.kind == TokKind::Word && ["TRAN", "TRANSACTION", "COMMIT", "ROLLBACK", "USE", "IMPLICIT_TRANSACTIONS", "XACT_ABORT"].iter().any(|w| t.text.eq_ignore_ascii_case(w))
    })
}

/// Split a script on `GO [count]` separator lines (outside strings and
/// comments). Returns `(batch, repeat count)`; empty batches are dropped.
pub fn split_batches(sql: &str) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_block = 0usize; // nested block comment level
    let mut in_string = false;
    let mut in_bracket = false;
    for line in sql.split_inclusive('\n') {
        if in_block == 0
            && !in_string
            && !in_bracket
            && let Some(count) = go_line(line)
        {
            if !current.trim().is_empty() {
                out.push((std::mem::take(&mut current), count));
            } else {
                current.clear();
            }
            continue;
        }
        // update lexical state for this line (byte-wise: all markers are
        // ASCII, so non-ASCII text never needs a char boundary)
        let b = line.as_bytes();
        let mut i = 0;
        while i < b.len() {
            let rest = &b[i..];
            if in_block > 0 {
                if rest.starts_with(b"*/") {
                    in_block -= 1;
                    i += 2;
                } else if rest.starts_with(b"/*") {
                    in_block += 1;
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            if in_string {
                if b[i] == b'\'' {
                    in_string = false;
                }
                i += 1;
                continue;
            }
            if in_bracket {
                if b[i] == b']' {
                    in_bracket = false;
                }
                i += 1;
                continue;
            }
            if rest.starts_with(b"--") {
                break;
            }
            if rest.starts_with(b"/*") {
                in_block += 1;
                i += 2;
                continue;
            }
            match b[i] {
                b'\'' => in_string = true,
                b'[' => in_bracket = true,
                _ => {}
            }
            i += 1;
        }
        current.push_str(line);
    }
    if !current.trim().is_empty() {
        out.push((current, 1));
    }
    out
}

/// `GO`, `GO 5`, `go -- comment` → repeat count.
fn go_line(line: &str) -> Option<u32> {
    let t = line.trim();
    let t = match t.find("--") {
        Some(i) => t[..i].trim(),
        None => t,
    };
    let mut parts = t.split_whitespace();
    let first = parts.next()?;
    if !first.eq_ignore_ascii_case("GO") {
        return None;
    }
    match parts.next() {
        None => Some(1),
        Some(n) => {
            if parts.next().is_some() {
                return None;
            }
            n.parse::<u32>().ok().filter(|c| *c > 0)
        }
    }
}

/// 1-based character position of the start of `line` (1-based) in `sql`.
pub fn line_start_position(sql: &str, line: u32) -> Option<usize> {
    if line == 0 {
        return None;
    }
    if line == 1 {
        return Some(1);
    }
    let mut current = 1u32;
    for (chars, (i, c)) in sql.char_indices().enumerate() {
        let _ = i;
        if c == '\n' {
            current += 1;
            if current == line {
                return Some(chars + 2);
            }
        }
    }
    None
}

/// Remove redundant enclosing parentheses: `((0))` → `0`, `(getdate())` →
/// `getdate()`, while `(a)+(b)` is kept.
pub fn strip_outer_parens(expr: &str) -> String {
    let mut s = expr.trim();
    loop {
        if !(s.starts_with('(') && s.ends_with(')')) {
            return s.to_string();
        }
        // does the first '(' close at the very end?
        let toks = tokenize(s);
        let closes_at_end = toks.iter().find(|t| t.kind == TokKind::Punct && t.text == ")" && t.depth == 0).map(|t| t.start == s.len() - 1).unwrap_or(false);
        if !closes_at_end {
            return s.to_string();
        }
        s = s[1..s.len() - 1].trim();
    }
}

/// For `CREATE [OR ALTER] VIEW name [(cols)] [WITH opts] AS select`, the
/// SELECT part.
pub fn view_select(definition: &str) -> Option<&str> {
    let toks = tokenize(definition);
    let view_pos = toks.iter().position(|t| t.is("VIEW"))?;
    let as_tok = toks.iter().skip(view_pos + 1).find(|t| t.depth == 0 && t.is("AS"))?;
    Some(definition[as_tok.end()..].trim())
}

/// Result of analysing a SELECT for row limiting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectShape {
    /// Statement is a (possibly CTE prefixed) SELECT.
    pub is_select: bool,
    /// Byte offset of the top-level `ORDER BY`, if any.
    pub order_by: Option<usize>,
    /// Top-level TOP / OFFSET / FOR / OPTION / INTO / UNION present.
    pub has_top: bool,
    /// `OFFSET` present.
    pub has_offset: bool,
    /// FOR XML / JSON / BROWSE or OPTION(...) present.
    pub has_trailer: bool,
    /// UNION / EXCEPT / INTERSECT at top level.
    pub has_set_op: bool,
    /// `SELECT ... INTO`.
    pub has_into: bool,
    /// Byte offset where `TOP (n)` can be inserted (after SELECT [ALL|DISTINCT]).
    pub top_insert_at: Option<usize>,
}

/// Inspect a statement for [`crate::dialect::MsSqlDialect::limit_query`].
pub fn select_shape(sql: &str) -> SelectShape {
    let toks = tokenize(sql);
    let mut shape = SelectShape {
        is_select: classify(sql) == StmtClass::Select,
        order_by: None,
        has_top: false,
        has_offset: false,
        has_trailer: false,
        has_set_op: false,
        has_into: false,
        top_insert_at: None,
    };
    let mut seen_main_select = false;
    for (i, t) in toks.iter().enumerate() {
        if t.depth != 0 || t.kind != TokKind::Word {
            continue;
        }
        let upper = t.text.to_ascii_uppercase();
        match upper.as_str() {
            "SELECT" if !seen_main_select => {
                seen_main_select = true;
                let mut at = t.end();
                if let Some(n) = toks.get(i + 1) {
                    if n.is("DISTINCT") || n.is("ALL") {
                        at = n.end();
                    }
                    if n.is("TOP") || toks.get(i + 2).is_some_and(|m| m.is("TOP")) {
                        shape.has_top = true;
                    }
                }
                shape.top_insert_at = Some(at);
            }
            "ORDER" if toks.get(i + 1).is_some_and(|n| n.is("BY")) => shape.order_by = Some(t.start),
            "OFFSET" => shape.has_offset = true,
            "FOR" | "OPTION" => shape.has_trailer = true,
            "UNION" | "EXCEPT" | "INTERSECT" => shape.has_set_op = true,
            "INTO" if seen_main_select => shape.has_into = true,
            _ => {}
        }
    }
    shape
}

/// Unquote `[a]]b]` / `"x"` identifiers.
pub fn unquote_ident(s: &str) -> String {
    let s = s.trim();
    if let Some(inner) = s.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        return inner.replace("]]", "]");
    }
    if let Some(inner) = s.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        return inner.replace("\"\"", "\"");
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens() {
        let t = tokenize("SELECT [a]]b], N'it''s', @x, @@SPID, #tmp, 0x1F, 1.5e-3 /* c /* n */ */ -- x\n FROM t");
        let texts: Vec<&str> = t.iter().map(|t| t.text).collect();
        assert_eq!(texts, vec!["SELECT", "[a]]b]", ",", "N'it''s'", ",", "@x", ",", "@@SPID", ",", "#tmp", ",", "0x1F", ",", "1.5e-3", "FROM", "t"]);
        assert_eq!(t[3].kind, TokKind::Str);
        assert_eq!(t[7].kind, TokKind::Variable);
        let d = tokenize("f(a, (b))");
        assert_eq!(d.iter().find(|t| t.text == "b").unwrap().depth, 2);
        assert_eq!(d.last().unwrap().depth, 0);
    }

    #[test]
    fn classification() {
        assert_eq!(classify("select 1"), StmtClass::Select);
        assert_eq!(classify("  -- c\nUPDATE t SET a = 1"), StmtClass::Dml);
        assert_eq!(classify("WITH x AS (SELECT 1 a) DELETE FROM t WHERE a IN (SELECT a FROM x)"), StmtClass::Dml);
        assert_eq!(classify("WITH x AS (SELECT 1 a) SELECT * FROM x"), StmtClass::Select);
        assert_eq!(classify("EXEC sp_who"), StmtClass::Other);
        assert_eq!(classify("(SELECT 1) UNION (SELECT 2)"), StmtClass::Select);
        assert_eq!(classify("CREATE TABLE t (a int)"), StmtClass::Other);
        assert_eq!(first_keyword("/* x */ merge INTO t"), Some("MERGE".into()));
    }

    #[test]
    fn scope_sensitivity() {
        assert!(!is_scope_sensitive("UPDATE t SET a = 1 WHERE b = 2"));
        assert!(!is_scope_sensitive("INSERT INTO #t VALUES (1)"));
        assert!(!is_scope_sensitive("DELETE FROM t; SET @x = 1"));
        assert!(is_scope_sensitive("SET NOCOUNT ON; UPDATE t SET a = 1"));
        assert!(is_scope_sensitive("USE master"));
        assert!(is_scope_sensitive("BEGIN TRAN; UPDATE t SET a = 1"));
        assert!(is_scope_sensitive("UPDATE t SET a = 1; COMMIT"));
        assert!(is_scope_sensitive("SELECT a INTO #x FROM t"));
        assert!(is_scope_sensitive("CREATE TABLE #x (a int)"));
        assert!(is_scope_sensitive("EXECUTE AS USER = 'bob'"));
        assert!(!is_scope_sensitive("UPDATE t SET a = 'USE x; COMMIT'"));
    }

    #[test]
    fn batches() {
        let script = "SELECT 1\nGO\nSELECT 'GO\nGO'\ngo 3\n/* GO\nGO */\nSELECT 2 -- tail\nGO -- done\n\nGO\n";
        let b = split_batches(script);
        assert_eq!(b.len(), 3, "{b:?}");
        assert_eq!(b[0], ("SELECT 1\n".to_string(), 1));
        assert_eq!(b[1].1, 3);
        assert!(b[1].0.contains("GO'"));
        assert!(b[2].0.contains("SELECT 2"));
        assert_eq!(split_batches("SELECT 1").len(), 1);
        assert!(split_batches("GO\nGO").is_empty());
        assert_eq!(go_line("GOTO x"), None);
        // non-ASCII text in comments, identifiers and strings must not break the scanner
        let b = split_batches("SELECT é AS [ü] /* ç\n GO ğ */ -- ö\nGO\nSELECT N'ñ'");
        assert_eq!(b.len(), 2, "{b:?}");
        assert_eq!(go_line("go 2 x"), None);
    }

    #[test]
    fn positions_and_parens() {
        assert_eq!(line_start_position("a\nbé\nc", 3), Some(6));
        assert_eq!(line_start_position("a", 1), Some(1));
        assert_eq!(line_start_position("a", 4), None);
        assert_eq!(strip_outer_parens("((0))"), "0");
        assert_eq!(strip_outer_parens("(getdate())"), "getdate()");
        assert_eq!(strip_outer_parens("((1)+(2))"), "(1)+(2)");
        assert_eq!(strip_outer_parens("(N'a)b')"), "N'a)b'");
        assert_eq!(strip_outer_parens("([qty]>(0))"), "[qty]>(0)");
        assert_eq!(strip_outer_parens("x"), "x");
    }

    #[test]
    fn view_header() {
        assert_eq!(view_select("CREATE VIEW [dbo].[v] (a, b) WITH SCHEMABINDING AS\nSELECT a, b FROM t"), Some("SELECT a, b FROM t"));
        assert_eq!(view_select("create or alter view v as select 1 as [as]"), Some("select 1 as [as]"));
        assert_eq!(view_select("SELECT 1"), None);
    }

    #[test]
    fn shapes() {
        let s = select_shape("SELECT DISTINCT a FROM t ORDER BY a");
        assert!(s.is_select);
        assert!(s.order_by.is_some());
        assert_eq!(s.top_insert_at, Some("SELECT DISTINCT".len()));
        let s = select_shape("SELECT TOP 5 a FROM t");
        assert!(s.has_top);
        let s = select_shape("SELECT a FROM (SELECT a FROM u ORDER BY a OFFSET 0 ROWS) x");
        assert!(s.order_by.is_none());
        assert!(!s.has_offset);
        let s = select_shape("SELECT a FROM t UNION ALL SELECT b FROM u");
        assert!(s.has_set_op);
        let s = select_shape("SELECT a FROM t FOR XML PATH('')");
        assert!(s.has_trailer);
        assert_eq!(unquote_ident("[a]]b]"), "a]b");
        assert_eq!(unquote_ident("\"x\"\"y\""), "x\"y");
        assert_eq!(unquote_ident("plain"), "plain");
    }

    #[test]
    fn session_state_words() {
        assert!(touches_session_state("BEGIN TRANSACTION"));
        assert!(touches_session_state("use db1"));
        assert!(!touches_session_state("SELECT 'COMMIT'"));
    }
}
