//! A small SQLite compatible lexer.
//!
//! It follows the token rules of SQLite's `tokenize.c` closely enough to
//! split scripts into statements (including `CREATE TRIGGER ... BEGIN ... END`
//! bodies, exactly like `sqlite3_complete()`), to classify statements and to
//! parse the `CREATE` statements stored in `sqlite_schema`.

/// Kind of a lexical token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokKind {
    /// Whitespace.
    Space,
    /// `-- ...` or `/* ... */`.
    Comment,
    /// Bare identifier or keyword.
    Word,
    /// `"..."`, `` `...` `` or `[...]`.
    QuotedIdent,
    /// `'...'`.
    String,
    /// `X'...'`.
    Blob,
    Number,
    /// `?`, `?NNN`, `:name`, `@name`, `$name`, `#name`.
    Variable,
    LParen,
    RParen,
    Comma,
    Semi,
    Dot,
    Operator,
    /// Unterminated string / identifier or a stray character.
    Illegal,
}

/// One token: kind, text and byte offset in the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Token<'a> {
    pub kind: TokKind,
    pub text: &'a str,
    pub start: usize,
}

impl<'a> Token<'a> {
    /// Byte offset just past the token.
    pub fn end(&self) -> usize {
        self.start + self.text.len()
    }

    /// Whitespace or comment.
    pub fn is_trivia(&self) -> bool {
        matches!(self.kind, TokKind::Space | TokKind::Comment)
    }

    /// Bare word equal to `kw` (ASCII case-insensitive).
    pub fn is_word(&self, kw: &str) -> bool {
        self.kind == TokKind::Word && self.text.eq_ignore_ascii_case(kw)
    }

    /// Bare word equal to one of `kws`.
    pub fn is_any_word(&self, kws: &[&str]) -> bool {
        self.kind == TokKind::Word && kws.iter().any(|k| self.text.eq_ignore_ascii_case(k))
    }

    /// Value of a name token: bare words, quoted identifiers and (as SQLite
    /// accepts them in name positions) string literals.
    pub fn ident(&self) -> Option<String> {
        match self.kind {
            TokKind::Word => Some(self.text.to_string()),
            TokKind::QuotedIdent | TokKind::String => Some(unquote(self.text)),
            _ => None,
        }
    }
}

/// Remove the quotes of a quoted identifier or string literal, resolving
/// doubled quote characters.
pub(crate) fn unquote(text: &str) -> String {
    let mut chars = text.chars();
    let Some(open) = chars.next() else {
        return String::new();
    };
    let close = match open {
        '"' => '"',
        '\'' => '\'',
        '`' => '`',
        '[' => ']',
        _ => return text.to_string(),
    };
    let inner = &text[open.len_utf8()..];
    let inner = inner.strip_suffix(close).unwrap_or(inner);
    if open == '[' {
        return inner.to_string();
    }
    let doubled: String = [close, close].iter().collect();
    inner.replace(&doubled, &close.to_string())
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0c)
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c >= 0x80
}

fn is_ident_char(c: u8) -> bool {
    is_ident_start(c) || c.is_ascii_digit() || c == b'$'
}

/// Scan a quoted token starting at `*i` (on the opening quote). Doubled quote
/// characters are escapes. Returns false when unterminated.
fn scan_quoted(b: &[u8], i: &mut usize, quote: u8) -> bool {
    *i += 1;
    while *i < b.len() {
        if b[*i] == quote {
            if b.get(*i + 1) == Some(&quote) {
                *i += 2;
                continue;
            }
            *i += 1;
            return true;
        }
        *i += 1;
    }
    false
}

fn scan_number(b: &[u8], i: &mut usize) {
    let digit = |c: Option<&u8>| c.is_some_and(|c| c.is_ascii_digit());
    if b[*i] == b'0'
        && matches!(b.get(*i + 1), Some(b'x' | b'X'))
        && b.get(*i + 2).is_some_and(|c| c.is_ascii_hexdigit())
    {
        *i += 2;
        while *i < b.len() && (b[*i].is_ascii_hexdigit() || b[*i] == b'_') {
            *i += 1;
        }
        return;
    }
    while *i < b.len() && (b[*i].is_ascii_digit() || b[*i] == b'_') {
        *i += 1;
    }
    if *i < b.len() && b[*i] == b'.' {
        *i += 1;
        while *i < b.len() && (b[*i].is_ascii_digit() || b[*i] == b'_') {
            *i += 1;
        }
    }
    if *i < b.len() && matches!(b[*i], b'e' | b'E') {
        if digit(b.get(*i + 1)) {
            *i += 1;
        } else if matches!(b.get(*i + 1), Some(b'+' | b'-')) && digit(b.get(*i + 2)) {
            *i += 2;
        } else {
            return;
        }
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
    }
}

/// Split `sql` into tokens, including whitespace and comments. The tokens
/// cover the input completely and in order.
pub(crate) fn tokenize(sql: &str) -> Vec<Token<'_>> {
    let b = sql.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 3 + 1);
    let mut i = 0usize;
    while i < b.len() {
        let start = i;
        let c = b[i];
        let next = b.get(i + 1).copied();
        let kind = match c {
            c if is_space(c) => {
                while i < b.len() && is_space(b[i]) {
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
            b'-' if next == Some(b'>') => {
                i += 2;
                if b.get(i) == Some(&b'>') {
                    i += 1;
                }
                TokKind::Operator
            }
            b'/' if next == Some(b'*') => {
                i += 2;
                loop {
                    if i >= b.len() {
                        break;
                    }
                    if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                TokKind::Comment
            }
            b'(' => {
                i += 1;
                TokKind::LParen
            }
            b')' => {
                i += 1;
                TokKind::RParen
            }
            b',' => {
                i += 1;
                TokKind::Comma
            }
            b';' => {
                i += 1;
                TokKind::Semi
            }
            b'\'' => {
                if scan_quoted(b, &mut i, b'\'') {
                    TokKind::String
                } else {
                    TokKind::Illegal
                }
            }
            b'"' | b'`' => {
                if scan_quoted(b, &mut i, c) {
                    TokKind::QuotedIdent
                } else {
                    TokKind::Illegal
                }
            }
            b'[' => {
                i += 1;
                while i < b.len() && b[i] != b']' {
                    i += 1;
                }
                if i < b.len() {
                    i += 1;
                    TokKind::QuotedIdent
                } else {
                    TokKind::Illegal
                }
            }
            b'x' | b'X' if next == Some(b'\'') => {
                i += 1;
                if scan_quoted(b, &mut i, b'\'') {
                    TokKind::Blob
                } else {
                    TokKind::Illegal
                }
            }
            b'0'..=b'9' => {
                scan_number(b, &mut i);
                TokKind::Number
            }
            b'.' if next.is_some_and(|n| n.is_ascii_digit()) => {
                scan_number(b, &mut i);
                TokKind::Number
            }
            b'.' => {
                i += 1;
                TokKind::Dot
            }
            b'?' => {
                i += 1;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                TokKind::Variable
            }
            b'$' | b'@' | b':' | b'#' if next.is_some_and(is_ident_char) => {
                i += 1;
                while i < b.len() {
                    if is_ident_char(b[i]) {
                        i += 1;
                    } else if c == b'$' && b[i] == b':' && b.get(i + 1) == Some(&b':') {
                        i += 2;
                    } else {
                        break;
                    }
                }
                TokKind::Variable
            }
            c if is_ident_start(c) => {
                while i < b.len() && is_ident_char(b[i]) {
                    i += 1;
                }
                TokKind::Word
            }
            b'=' => {
                i += if next == Some(b'=') { 2 } else { 1 };
                TokKind::Operator
            }
            b'<' => {
                i += if matches!(next, Some(b'=' | b'>' | b'<')) {
                    2
                } else {
                    1
                };
                TokKind::Operator
            }
            b'>' => {
                i += if matches!(next, Some(b'=' | b'>')) {
                    2
                } else {
                    1
                };
                TokKind::Operator
            }
            b'!' => {
                if next == Some(b'=') {
                    i += 2;
                    TokKind::Operator
                } else {
                    i += 1;
                    TokKind::Illegal
                }
            }
            b'|' => {
                i += if next == Some(b'|') { 2 } else { 1 };
                TokKind::Operator
            }
            _ => {
                // ASCII punctuation (non-ASCII bytes are identifier characters).
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
    /// Statement text from its first significant token through the
    /// terminating semicolon (when present).
    pub text: &'a str,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SplitState {
    Start,
    Normal,
    Explain,
    Create,
    Trigger,
    Semi,
    End,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SplitTok {
    Semi,
    Other,
    Explain,
    Create,
    Temp,
    Trigger,
    End,
}

/// The state machine of `sqlite3_complete()`: a `;` only ends a statement
/// when it is not inside a `CREATE TRIGGER` body.
fn split_transition(state: SplitState, tok: SplitTok) -> SplitState {
    use SplitState as S;
    use SplitTok as T;
    match (state, tok) {
        (S::Trigger, T::Semi) | (S::Semi, T::Semi) => S::Semi,
        (S::Semi, T::End) => S::End,
        (S::End, T::Semi) => S::Start,
        (S::Trigger | S::Semi | S::End, _) => S::Trigger,
        (_, T::Semi) => S::Start,
        (S::Start, T::Explain) => S::Explain,
        (S::Start | S::Explain, T::Create) => S::Create,
        (S::Start, _) => S::Normal,
        (S::Normal, _) => S::Normal,
        (S::Explain, T::Other) => S::Explain,
        (S::Explain, _) => S::Normal,
        (S::Create, T::Temp) => S::Create,
        (S::Create, T::Trigger) => S::Trigger,
        (S::Create, _) => S::Normal,
    }
}

/// Split a script into statements. Empty statements (lone `;`, comments
/// only) are dropped.
pub(crate) fn split_statements(sql: &str) -> Vec<StatementSpan<'_>> {
    let mut out = Vec::new();
    let mut state = SplitState::Start;
    let mut stmt_start: Option<usize> = None;
    for t in tokenize(sql) {
        if t.is_trivia() {
            continue;
        }
        let class = match t.kind {
            TokKind::Semi => SplitTok::Semi,
            TokKind::Word => {
                if t.is_word("EXPLAIN") {
                    SplitTok::Explain
                } else if t.is_word("CREATE") {
                    SplitTok::Create
                } else if t.is_any_word(&["TEMP", "TEMPORARY"]) {
                    SplitTok::Temp
                } else if t.is_word("TRIGGER") {
                    SplitTok::Trigger
                } else if t.is_word("END") {
                    SplitTok::End
                } else {
                    SplitTok::Other
                }
            }
            _ => SplitTok::Other,
        };
        if class != SplitTok::Semi && stmt_start.is_none() {
            stmt_start = Some(t.start);
        }
        state = split_transition(state, class);
        if class == SplitTok::Semi
            && state == SplitState::Start
            && let Some(s) = stmt_start.take()
        {
            out.push(StatementSpan {
                offset: s,
                text: &sql[s..t.end()],
            });
        }
    }
    if let Some(s) = stmt_start {
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
    Select,
    Insert,
    Update,
    Delete,
    Begin,
    Commit,
    Rollback,
    Savepoint,
    Release,
    Pragma,
    Attach,
    Detach,
    Vacuum,
    Explain,
    Analyze,
    Reindex,
    /// `CREATE ...`, `DROP ...`, `ALTER ...` with the command tag.
    Ddl(String),
    Other(String),
}

impl StatementKind {
    /// Data modifying statements reporting an affected row count.
    pub fn is_dml(&self) -> bool {
        matches!(
            self,
            StatementKind::Insert | StatementKind::Update | StatementKind::Delete
        )
    }

    /// Statements controlling the transaction itself.
    pub fn is_transaction_control(&self) -> bool {
        matches!(
            self,
            StatementKind::Begin
                | StatementKind::Commit
                | StatementKind::Rollback
                | StatementKind::Savepoint
                | StatementKind::Release
        )
    }

    /// Statements that must not be wrapped into an implicit transaction
    /// (manual commit mode): transaction control, PRAGMAs (several cannot
    /// change inside a transaction), VACUUM, ATTACH / DETACH and EXPLAIN.
    pub fn runs_outside_transaction(&self) -> bool {
        self.is_transaction_control()
            || matches!(
                self,
                StatementKind::Pragma
                    | StatementKind::Vacuum
                    | StatementKind::Attach
                    | StatementKind::Detach
                    | StatementKind::Explain
            )
    }

    /// Command tag reported in [`dbiji_core::ResultHeader::Update`].
    pub fn tag(&self) -> String {
        match self {
            StatementKind::Select => "SELECT".into(),
            StatementKind::Insert => "INSERT".into(),
            StatementKind::Update => "UPDATE".into(),
            StatementKind::Delete => "DELETE".into(),
            StatementKind::Begin => "BEGIN".into(),
            StatementKind::Commit => "COMMIT".into(),
            StatementKind::Rollback => "ROLLBACK".into(),
            StatementKind::Savepoint => "SAVEPOINT".into(),
            StatementKind::Release => "RELEASE".into(),
            StatementKind::Pragma => "PRAGMA".into(),
            StatementKind::Attach => "ATTACH".into(),
            StatementKind::Detach => "DETACH".into(),
            StatementKind::Vacuum => "VACUUM".into(),
            StatementKind::Explain => "EXPLAIN".into(),
            StatementKind::Analyze => "ANALYZE".into(),
            StatementKind::Reindex => "REINDEX".into(),
            StatementKind::Ddl(t) | StatementKind::Other(t) => t.clone(),
        }
    }
}

/// Main verb of a `WITH ...` statement: the first DML keyword at nesting level 0.
fn with_main_verb(toks: &[Token<'_>]) -> StatementKind {
    let mut depth = 0i32;
    for t in toks.iter().skip(1) {
        match t.kind {
            TokKind::LParen => depth += 1,
            TokKind::RParen => depth -= 1,
            TokKind::Word if depth == 0 => {
                if t.is_any_word(&["SELECT", "VALUES"]) {
                    return StatementKind::Select;
                } else if t.is_any_word(&["INSERT", "REPLACE"]) {
                    return StatementKind::Insert;
                } else if t.is_word("UPDATE") {
                    return StatementKind::Update;
                } else if t.is_word("DELETE") {
                    return StatementKind::Delete;
                }
            }
            _ => {}
        }
    }
    StatementKind::Other("WITH".into())
}

/// Classify one statement by its leading keywords.
pub(crate) fn classify(stmt: &str) -> StatementKind {
    let toks = significant(stmt);
    let Some(first) = toks.first() else {
        return StatementKind::Other(String::new());
    };
    if first.kind != TokKind::Word {
        return StatementKind::Other(String::new());
    }
    let word = first.text.to_ascii_uppercase();
    match word.as_str() {
        "SELECT" | "VALUES" => StatementKind::Select,
        "INSERT" | "REPLACE" => StatementKind::Insert,
        "UPDATE" => StatementKind::Update,
        "DELETE" => StatementKind::Delete,
        "WITH" => with_main_verb(&toks),
        "BEGIN" => StatementKind::Begin,
        "COMMIT" | "END" => StatementKind::Commit,
        "ROLLBACK" => StatementKind::Rollback,
        "SAVEPOINT" => StatementKind::Savepoint,
        "RELEASE" => StatementKind::Release,
        "PRAGMA" => StatementKind::Pragma,
        "ATTACH" => StatementKind::Attach,
        "DETACH" => StatementKind::Detach,
        "VACUUM" => StatementKind::Vacuum,
        "EXPLAIN" => StatementKind::Explain,
        "ANALYZE" => StatementKind::Analyze,
        "REINDEX" => StatementKind::Reindex,
        "CREATE" | "DROP" | "ALTER" => {
            let object = toks
                .iter()
                .skip(1)
                .filter(|t| t.kind == TokKind::Word)
                .find(|t| t.is_any_word(&["TABLE", "INDEX", "VIEW", "TRIGGER"]))
                .map(|t| t.text.to_ascii_uppercase());
            let virtual_ = toks.iter().take(4).any(|t| t.is_word("VIRTUAL"));
            match object {
                Some(o) if virtual_ => StatementKind::Ddl(format!("{word} VIRTUAL {o}")),
                Some(o) => StatementKind::Ddl(format!("{word} {o}")),
                None => StatementKind::Ddl(word),
            }
        }
        _ => StatementKind::Other(word),
    }
}

/// True when the text contains nothing but whitespace and comments.
#[cfg(test)]
pub(crate) fn is_blank(sql: &str) -> bool {
    tokenize(sql).iter().all(Token::is_trivia)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(sql: &str) -> Vec<TokKind> {
        significant(sql).iter().map(|t| t.kind).collect()
    }

    #[test]
    fn tokens_cover_input() {
        let sql = "SELECT a, \"b\"\"c\", [d e], `f`, 'it''s', x'0A', 1.5e3, .5, 0x1F, ?1, :n, @v, $t \
                   FROM t -- comment\n /* block */ WHERE a->>'$.x' <> 3 || 'y';";
        let toks = tokenize(sql);
        let joined: String = toks.iter().map(|t| t.text).collect();
        assert_eq!(joined, sql);
        assert!(toks.iter().all(|t| t.kind != TokKind::Illegal), "{toks:?}");
    }

    #[test]
    fn token_kinds() {
        use TokKind::*;
        assert_eq!(
            kinds("a \"b\" 'c' X'00' 12 ?3 :x ( ) , ; . ->> <="),
            vec![
                Word,
                QuotedIdent,
                String,
                Blob,
                Number,
                Variable,
                Variable,
                LParen,
                RParen,
                Comma,
                Semi,
                Dot,
                Operator,
                Operator
            ]
        );
        assert_eq!(kinds("'unterminated"), vec![Illegal]);
        assert_eq!(kinds("1_000_000"), vec![Number]);
        assert_eq!(kinds("t.c"), vec![Word, Dot, Word]);
    }

    #[test]
    fn unquoting() {
        assert_eq!(unquote("\"a\"\"b\""), "a\"b");
        assert_eq!(unquote("[a b]"), "a b");
        assert_eq!(unquote("`x``y`"), "x`y");
        assert_eq!(unquote("'it''s'"), "it's");
        assert_eq!(unquote("plain"), "plain");
    }

    #[test]
    fn split_simple() {
        let s = split_statements("SELECT 1; SELECT 'a;b' ; -- c\n;; /* x */ SELECT 3");
        let texts: Vec<&str> = s.iter().map(|s| s.text).collect();
        assert_eq!(texts, vec!["SELECT 1;", "SELECT 'a;b' ;", "SELECT 3"]);
        assert_eq!(s[1].offset, 10);
    }

    #[test]
    fn split_trigger_body() {
        let sql = "CREATE TRIGGER tr AFTER INSERT ON t BEGIN\n  UPDATE t SET a = CASE WHEN 1 THEN 2 END;\n  INSERT INTO log VALUES (1);\nEND;\nSELECT 2;";
        let s = split_statements(sql);
        assert_eq!(s.len(), 2, "{s:?}");
        assert!(s[0].text.ends_with("END;"));
        assert_eq!(s[1].text, "SELECT 2;");
        let temp = split_statements(
            "CREATE TEMP TRIGGER x BEFORE DELETE ON t BEGIN SELECT 1; END; SELECT 3",
        );
        assert_eq!(temp.len(), 2);
    }

    #[test]
    fn split_blank() {
        assert!(split_statements("  -- only a comment\n /* and this */ ;").is_empty());
        assert!(is_blank(" -- x\n"));
        assert!(!is_blank("select"));
    }

    #[test]
    fn classification() {
        assert_eq!(classify("select 1"), StatementKind::Select);
        assert_eq!(
            classify("  -- c\n insert into t values (1)"),
            StatementKind::Insert
        );
        assert_eq!(classify("REPLACE INTO t VALUES (1)"), StatementKind::Insert);
        assert_eq!(
            classify("WITH x AS (SELECT 1) DELETE FROM t WHERE a IN x"),
            StatementKind::Delete
        );
        assert_eq!(
            classify(
                "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c) SELECT x FROM c"
            ),
            StatementKind::Select
        );
        assert_eq!(classify("end transaction"), StatementKind::Commit);
        assert_eq!(
            classify("CREATE UNIQUE INDEX i ON t(a)"),
            StatementKind::Ddl("CREATE INDEX".into())
        );
        assert_eq!(
            classify("create virtual table f using fts5(a)"),
            StatementKind::Ddl("CREATE VIRTUAL TABLE".into())
        );
        assert_eq!(classify("PRAGMA foreign_keys = OFF").tag(), "PRAGMA");
        assert!(classify("savepoint a").is_transaction_control());
        assert!(classify("vacuum").runs_outside_transaction());
        assert!(!classify("select 1").runs_outside_transaction());
    }
}
