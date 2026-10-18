//! Lightweight lexical analysis of PostgreSQL statement text.
//!
//! The driver never parses SQL completely; it only needs to know a few facts
//! about a statement before sending it: whether it controls the transaction,
//! whether it may return rows (so a metadata `Parse`/`Describe` round trip is
//! worth it), whether it cannot run inside a transaction block, which command
//! tag the server will report and whether it is a `COPY` using the client
//! stream. The tokenizer understands every PostgreSQL quoting form (standard
//! and `E''` strings, quoted identifiers, dollar quoting, nested block
//! comments) so keywords inside literals are never misinterpreted.

/// One lexical token. Only the information needed for classification is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tok<'a> {
    /// Unquoted identifier or keyword (original spelling).
    Word(&'a str),
    /// `"quoted identifier"` (raw text including the quotes).
    Quoted(&'a str),
    /// Any string literal (standard, `E''`, `B''`, `X''`, `N''`, `U&''`, dollar quoted).
    Str,
    /// Numeric literal.
    Num,
    /// `$n` positional parameter.
    Param,
    /// `;`
    Semi,
    /// `(` or `[`
    Open,
    /// `)` or `]`
    Close,
    /// `,`
    Comma,
    /// `.`
    Dot,
    /// Any other operator character sequence.
    Op,
}

/// Token with its byte span in the source text.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Spanned<'a> {
    pub tok: Tok<'a>,
    pub start: usize,
    pub end: usize,
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

fn is_ident_char(c: char) -> bool {
    c == '_' || c == '$' || c.is_alphanumeric()
}

/// Tokenize `sql`, skipping whitespace and comments. Unterminated literals
/// or comments simply extend to the end of the text.
pub(crate) fn tokenize(sql: &str) -> Vec<Spanned<'_>> {
    let mut out = Vec::new();
    let bytes = sql.as_bytes();
    let mut i = 0usize;
    let len = bytes.len();
    while i < len {
        let c = match sql[i..].chars().next() {
            Some(c) => c,
            None => break,
        };
        let start = i;
        // whitespace
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        // line comment
        if c == '-' && bytes.get(i + 1) == Some(&b'-') {
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // nested block comment
        if c == '/' && bytes.get(i + 1) == Some(&b'*') {
            let mut depth = 0usize;
            while i < len {
                if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    depth = depth.saturating_sub(1);
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            continue;
        }
        // string literal prefixes: E'..', B'..', X'..', N'..', U&'..'
        if matches!(c, 'e' | 'E' | 'b' | 'B' | 'x' | 'X' | 'n' | 'N') && bytes.get(i + 1) == Some(&b'\'') {
            let backslash = matches!(c, 'e' | 'E');
            i = skip_string(bytes, i + 1, backslash);
            out.push(Spanned { tok: Tok::Str, start, end: i });
            continue;
        }
        if matches!(c, 'u' | 'U') && bytes.get(i + 1) == Some(&b'&') {
            if bytes.get(i + 2) == Some(&b'\'') {
                i = skip_string(bytes, i + 2, false);
                out.push(Spanned { tok: Tok::Str, start, end: i });
                continue;
            }
            if bytes.get(i + 2) == Some(&b'"') {
                i = skip_quoted_ident(bytes, i + 2);
                out.push(Spanned { tok: Tok::Quoted(&sql[start + 2..i]), start, end: i });
                continue;
            }
        }
        if c == '\'' {
            i = skip_string(bytes, i, false);
            out.push(Spanned { tok: Tok::Str, start, end: i });
            continue;
        }
        if c == '"' {
            i = skip_quoted_ident(bytes, i);
            out.push(Spanned { tok: Tok::Quoted(&sql[start..i]), start, end: i });
            continue;
        }
        if c == '$' {
            // positional parameter
            if bytes.get(i + 1).is_some_and(|b| b.is_ascii_digit()) {
                i += 1;
                while i < len && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                out.push(Spanned { tok: Tok::Param, start, end: i });
                continue;
            }
            // dollar quote $tag$ ... $tag$
            if let Some(tag_end) = dollar_tag_end(sql, i) {
                let tag = &sql[i..tag_end];
                let body_start = tag_end;
                match sql[body_start..].find(tag) {
                    Some(pos) => i = body_start + pos + tag.len(),
                    None => i = len,
                }
                out.push(Spanned { tok: Tok::Str, start, end: i });
                continue;
            }
            i += 1;
            out.push(Spanned { tok: Tok::Op, start, end: i });
            continue;
        }
        if is_ident_start(c) {
            i += c.len_utf8();
            while i < len {
                let Some(n) = sql[i..].chars().next() else { break };
                if is_ident_char(n) {
                    i += n.len_utf8();
                } else {
                    break;
                }
            }
            out.push(Spanned { tok: Tok::Word(&sql[start..i]), start, end: i });
            continue;
        }
        if c.is_ascii_digit() || (c == '.' && bytes.get(i + 1).is_some_and(|b| b.is_ascii_digit())) {
            i += 1;
            while i < len {
                let b = bytes[i];
                if b.is_ascii_digit() || b == b'.' || b == b'_' {
                    i += 1;
                } else if (b == b'e' || b == b'E')
                    && (bytes.get(i + 1).is_some_and(|n| n.is_ascii_digit())
                        || (matches!(bytes.get(i + 1), Some(b'+') | Some(b'-')) && bytes.get(i + 2).is_some_and(|n| n.is_ascii_digit())))
                {
                    i += 2;
                } else {
                    break;
                }
            }
            out.push(Spanned { tok: Tok::Num, start, end: i });
            continue;
        }
        i += c.len_utf8();
        let tok = match c {
            ';' => Tok::Semi,
            '(' | '[' => Tok::Open,
            ')' | ']' => Tok::Close,
            ',' => Tok::Comma,
            '.' => Tok::Dot,
            _ => Tok::Op,
        };
        out.push(Spanned { tok, start, end: i });
    }
    out
}

/// Position after a `'...'` literal starting at `i` (which points at the quote).
fn skip_string(bytes: &[u8], mut i: usize, backslash: bool) -> usize {
    i += 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if backslash => i += 2,
            b'\'' => {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                } else {
                    return i + 1;
                }
            }
            _ => i += 1,
        }
    }
    bytes.len()
}

fn skip_quoted_ident(bytes: &[u8], mut i: usize) -> usize {
    i += 1;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            if bytes.get(i + 1) == Some(&b'"') {
                i += 2;
            } else {
                return i + 1;
            }
        } else {
            i += 1;
        }
    }
    bytes.len()
}

/// If a dollar quote tag (`$$` or `$tag$`) starts at `i`, return the byte
/// position just after the opening tag.
fn dollar_tag_end(sql: &str, i: usize) -> Option<usize> {
    let rest = &sql[i + 1..];
    for (k, ch) in rest.char_indices() {
        if ch == '$' {
            return Some(i + 1 + k + 1);
        }
        let ok = if k == 0 { ch == '_' || ch.is_alphabetic() } else { ch == '_' || ch.is_alphanumeric() };
        if !ok {
            return None;
        }
    }
    None
}

/// Split a script into statements at top-level semicolons (outside literals,
/// comments, parentheses and `BEGIN ... END` bodies of SQL-standard function
/// definitions). Returns trimmed, non-empty statement texts without the
/// terminating semicolon.
pub(crate) fn split_statements(sql: &str) -> Vec<&str> {
    let toks = tokenize(sql);
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut begin_depth = 0i32;
    let mut stmt_start: Option<usize> = None;
    let mut stmt_tokens = 0usize;
    let mut words: Vec<String> = Vec::new();
    let mut last_end = 0usize;
    for t in &toks {
        match t.tok {
            Tok::Semi if depth <= 0 && begin_depth <= 0 => {
                if let Some(s) = stmt_start.take() {
                    let text = sql[s..last_end].trim();
                    if !text.is_empty() {
                        out.push(text);
                    }
                }
                stmt_tokens = 0;
                words.clear();
                depth = 0;
                begin_depth = 0;
                continue;
            }
            Tok::Open => depth += 1,
            Tok::Close => depth -= 1,
            Tok::Word(w) => {
                if words.len() < 6 {
                    words.push(w.to_ascii_uppercase());
                }
                let is_routine = is_create_routine(&words);
                if is_routine && stmt_tokens > 1 {
                    if w.eq_ignore_ascii_case("begin") || w.eq_ignore_ascii_case("case") {
                        begin_depth += 1;
                    } else if w.eq_ignore_ascii_case("end") && begin_depth > 0 {
                        begin_depth -= 1;
                    }
                }
            }
            _ => {}
        }
        if stmt_start.is_none() {
            stmt_start = Some(t.start);
        }
        stmt_tokens += 1;
        last_end = t.end;
    }
    if let Some(s) = stmt_start {
        let text = sql[s..last_end].trim();
        if !text.is_empty() {
            out.push(text);
        }
    }
    out
}

fn is_create_routine(words: &[String]) -> bool {
    let mut it = words.iter().map(String::as_str);
    if it.next() != Some("CREATE") {
        return false;
    }
    let mut w = it.next();
    if w == Some("OR") {
        it.next(); // REPLACE
        w = it.next();
    }
    matches!(w, Some("FUNCTION") | Some("PROCEDURE"))
}

/// Effect of a statement on the session's transaction state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TxEffect {
    /// Ordinary statement.
    None,
    /// `BEGIN` / `START TRANSACTION`.
    Begin,
    /// `COMMIT` / `END` (optionally `AND CHAIN`).
    Commit { chain: bool },
    /// `ROLLBACK` / `ABORT` (optionally `AND CHAIN`).
    Rollback { chain: bool },
    /// `PREPARE TRANSACTION 'id'` (ends the session's transaction).
    PrepareTransaction,
    /// `SAVEPOINT`, `RELEASE`, `ROLLBACK TO`: need an open transaction, keep it open.
    Savepoint,
    /// `COMMIT PREPARED` / `ROLLBACK PREPARED`: not allowed inside a transaction block.
    FinishPrepared,
}

impl TxEffect {
    /// The statement itself controls the transaction (no implicit `BEGIN`).
    pub(crate) fn is_transaction_control(self) -> bool {
        !matches!(self, TxEffect::None | TxEffect::Savepoint)
    }
}

/// `COPY` using the client connection as data source or sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopyKind {
    None,
    FromStdin,
    ToStdout,
}

/// Classification of one statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StmtInfo {
    /// First keyword, upper case (`""` when the statement has none).
    pub verb: String,
    /// Main command after leading `WITH` clauses (`INSERT` for `WITH ... INSERT`).
    pub main_verb: String,
    pub tx: TxEffect,
    /// Cannot run inside a transaction block (VACUUM, CREATE DATABASE, ...).
    pub needs_autocommit: bool,
    /// The statement can produce a row description.
    pub may_return_rows: bool,
    /// The command tag carries a row count that means "rows affected".
    pub counts_rows: bool,
    /// Command tag prefix as reported by the server (`CREATE TABLE`, `INSERT`).
    pub tag: String,
    pub copy: CopyKind,
    /// Contains `$n` placeholders.
    pub has_params: bool,
}

impl StmtInfo {
    /// Command tag including the row count, mirroring the server's CommandComplete text.
    pub(crate) fn command_tag(&self, rows: u64) -> String {
        match self.tag.as_str() {
            "INSERT" => format!("INSERT 0 {rows}"),
            "UPDATE" | "DELETE" | "MERGE" | "SELECT" | "COPY" | "FETCH" | "MOVE" => {
                format!("{} {rows}", self.tag)
            }
            "" => String::new(),
            t => t.to_string(),
        }
    }

    /// Rows affected as reported to the application.
    pub(crate) fn rows_affected(&self, rows: u64) -> Option<u64> {
        self.counts_rows.then_some(rows)
    }
}

const NO_ROW_VERBS: &[&str] = &[
    "ALTER",
    "ANALYZE",
    "ANALYSE",
    "BEGIN",
    "CHECKPOINT",
    "CLOSE",
    "CLUSTER",
    "COMMENT",
    "COMMIT",
    "CREATE",
    "DEALLOCATE",
    "DECLARE",
    "DELETE",
    "DISCARD",
    "DO",
    "DROP",
    "END",
    "GRANT",
    "IMPORT",
    "INSERT",
    "LISTEN",
    "LOAD",
    "LOCK",
    "MERGE",
    "MOVE",
    "NOTIFY",
    "PREPARE",
    "REASSIGN",
    "REFRESH",
    "REINDEX",
    "RELEASE",
    "RESET",
    "REVOKE",
    "ROLLBACK",
    "ABORT",
    "SAVEPOINT",
    "SECURITY",
    "SET",
    "START",
    "TRUNCATE",
    "UNLISTEN",
    "UPDATE",
    "VACUUM",
];

const CTE_MAIN_VERBS: &[&str] = &["SELECT", "INSERT", "UPDATE", "DELETE", "MERGE", "VALUES", "TABLE"];

/// Modifiers skipped when building `CREATE`/`ALTER`/`DROP` command tags.
const TAG_MODIFIERS: &[&str] = &["OR", "REPLACE", "TEMP", "TEMPORARY", "UNLOGGED", "GLOBAL", "LOCAL", "UNIQUE", "RECURSIVE", "TRUSTED", "PROCEDURAL", "CONSTRAINT"];

/// Classify one statement (as returned by [`split_statements`]).
pub(crate) fn classify(stmt: &str) -> StmtInfo {
    let toks = tokenize(stmt);
    classify_tokens(&toks)
}

fn classify_tokens(toks: &[Spanned<'_>]) -> StmtInfo {
    // Leading words (upper case) at any depth, skipping leading parentheses.
    let mut words: Vec<String> = Vec::new();
    for t in toks.iter() {
        match t.tok {
            Tok::Word(w) => {
                words.push(w.to_ascii_uppercase());
                if words.len() >= 16 {
                    break;
                }
            }
            Tok::Open if words.is_empty() => continue,
            _ => {
                if words.is_empty() {
                    continue;
                }
                // Keep collecting words, other tokens are separators.
            }
        }
    }
    let has_params = toks.iter().any(|t| t.tok == Tok::Param);
    let verb = words.first().cloned().unwrap_or_default();
    let w = |i: usize| words.get(i).map(String::as_str).unwrap_or("");

    // depth-0 words in order (for WITH main verb, RETURNING, CREATE TABLE AS)
    let mut depth = 0i32;
    let mut top_words: Vec<String> = Vec::new();
    for t in toks {
        match t.tok {
            Tok::Open => depth += 1,
            Tok::Close => depth -= 1,
            Tok::Word(x) if depth <= 0 => top_words.push(x.to_ascii_uppercase()),
            _ => {}
        }
    }
    let main_verb =
        if verb == "WITH" { top_words.iter().skip(1).find(|x| CTE_MAIN_VERBS.contains(&x.as_str())).cloned().unwrap_or_else(|| "SELECT".to_string()) } else { verb.clone() };
    let has_returning = top_words.iter().any(|x| x == "RETURNING");

    let mut info = StmtInfo {
        verb: verb.clone(),
        main_verb: main_verb.clone(),
        tx: TxEffect::None,
        needs_autocommit: false,
        may_return_rows: true,
        counts_rows: false,
        tag: main_verb.clone(),
        copy: CopyKind::None,
        has_params,
    };

    // transaction control
    let chain = words.iter().any(|x| x == "CHAIN") && !contains_seq(&words, &["NO", "CHAIN"]);
    match verb.as_str() {
        "BEGIN" => {
            info.tx = TxEffect::Begin;
            info.tag = "BEGIN".into();
        }
        "START" if w(1) == "TRANSACTION" => {
            info.tx = TxEffect::Begin;
            info.tag = "START TRANSACTION".into();
        }
        "COMMIT" | "END" => {
            if w(1) == "PREPARED" {
                info.tx = TxEffect::FinishPrepared;
                info.needs_autocommit = true;
                info.tag = "COMMIT PREPARED".into();
            } else {
                info.tx = TxEffect::Commit { chain };
                info.tag = "COMMIT".into();
            }
        }
        "ROLLBACK" | "ABORT" => {
            let rest: Vec<&str> = words.iter().skip(1).map(String::as_str).collect();
            let to = rest.iter().take(2).any(|x| *x == "TO");
            if w(1) == "PREPARED" {
                info.tx = TxEffect::FinishPrepared;
                info.needs_autocommit = true;
                info.tag = "ROLLBACK PREPARED".into();
            } else if to {
                info.tx = TxEffect::Savepoint;
                info.tag = "ROLLBACK".into();
            } else {
                info.tx = TxEffect::Rollback { chain };
                info.tag = "ROLLBACK".into();
            }
        }
        "PREPARE" if w(1) == "TRANSACTION" => {
            info.tx = TxEffect::PrepareTransaction;
            info.tag = "PREPARE TRANSACTION".into();
        }
        "SAVEPOINT" => {
            info.tx = TxEffect::Savepoint;
            info.tag = "SAVEPOINT".into();
        }
        "RELEASE" => {
            info.tx = TxEffect::Savepoint;
            info.tag = "RELEASE".into();
        }
        _ => {}
    }

    // rows / tags
    match main_verb.as_str() {
        "INSERT" | "UPDATE" | "DELETE" | "MERGE" => {
            info.counts_rows = true;
            info.may_return_rows = has_returning;
        }
        "FETCH" => {
            info.counts_rows = true;
        }
        "MOVE" => {
            info.counts_rows = true;
            info.may_return_rows = false;
        }
        "SELECT" if verb == "SELECT" && top_words.iter().skip(1).any(|x| x == "INTO") => {
            // SELECT ... INTO new_table creates a table (no rows).
            info.counts_rows = true;
            info.may_return_rows = false;
        }
        "COPY" => {
            info.counts_rows = true;
            info.may_return_rows = false;
            let up: Vec<&str> = top_words.iter().map(String::as_str).collect();
            if contains_seq_str(&up, &["FROM", "STDIN"]) {
                info.copy = CopyKind::FromStdin;
            } else if contains_seq_str(&up, &["TO", "STDOUT"]) {
                info.copy = CopyKind::ToStdout;
            }
        }
        "CREATE" | "ALTER" | "DROP" => {
            info.may_return_rows = false;
            info.tag = ddl_tag(&words);
            if verb == "CREATE" {
                let obj = info.tag.clone();
                let obj = obj.as_str();
                let as_query = top_words.iter().skip(2).any(|x| x == "AS");
                if (obj == "CREATE TABLE" || obj == "CREATE MATERIALIZED VIEW") && as_query {
                    info.tag = "SELECT".into();
                    info.counts_rows = true;
                }
                if obj == "CREATE INDEX" && words.iter().take(4).any(|x| x == "CONCURRENTLY") {
                    info.needs_autocommit = true;
                }
                if matches!(obj, "CREATE DATABASE" | "CREATE TABLESPACE" | "CREATE SUBSCRIPTION") {
                    info.needs_autocommit = true;
                }
            } else if verb == "DROP" {
                let obj = info.tag.clone();
                let obj = obj.as_str();
                if matches!(obj, "DROP DATABASE" | "DROP TABLESPACE" | "DROP SUBSCRIPTION") {
                    info.needs_autocommit = true;
                }
                if obj == "DROP INDEX" && w(2) == "CONCURRENTLY" {
                    info.needs_autocommit = true;
                }
            } else {
                // ALTER
                if w(1) == "SYSTEM" {
                    info.needs_autocommit = true;
                }
                if w(1) == "DATABASE" && contains_seq(&words, &["SET", "TABLESPACE"]) {
                    info.needs_autocommit = true;
                }
                if w(1) == "SUBSCRIPTION" {
                    info.needs_autocommit = true;
                }
                if w(1) == "TABLE" && contains_seq(&top_words, &["DETACH", "PARTITION"]) && top_words.iter().any(|x| x == "CONCURRENTLY") {
                    info.needs_autocommit = true;
                }
            }
        }
        "VACUUM" => {
            info.may_return_rows = false;
            info.needs_autocommit = true;
        }
        "REINDEX" => {
            info.may_return_rows = false;
            let concurrently = words.iter().take(6).any(|x| x == "CONCURRENTLY");
            let whole = words.iter().skip(1).take(3).any(|x| x == "DATABASE" || x == "SYSTEM");
            info.needs_autocommit = concurrently || whole;
        }
        "CLUSTER" => {
            info.may_return_rows = false;
            // CLUSTER without a table name processes every table and cannot run in a transaction.
            let names = toks
                .iter()
                .filter(|t| matches!(t.tok, Tok::Word(_) | Tok::Quoted(_)))
                .filter(|t| !matches!(t.tok, Tok::Word(x) if x.eq_ignore_ascii_case("cluster") || x.eq_ignore_ascii_case("verbose")))
                .count();
            info.needs_autocommit = names == 0;
        }
        "DISCARD" => {
            info.may_return_rows = false;
            info.needs_autocommit = w(1) == "ALL";
            info.tag = if w(1) == "ALL" { "DISCARD ALL".into() } else { format!("DISCARD {}", w(1)) };
        }
        "TRUNCATE" => {
            info.may_return_rows = false;
            info.tag = "TRUNCATE TABLE".into();
        }
        "LOCK" => {
            info.may_return_rows = false;
            info.tag = "LOCK TABLE".into();
        }
        "DECLARE" => {
            info.may_return_rows = false;
            info.tag = "DECLARE CURSOR".into();
        }
        "CLOSE" => {
            info.may_return_rows = false;
            info.tag = if w(1) == "ALL" { "CLOSE CURSOR ALL".into() } else { "CLOSE CURSOR".into() };
        }
        "REFRESH" => {
            info.may_return_rows = false;
            info.tag = "REFRESH MATERIALIZED VIEW".into();
        }
        "SECURITY" => {
            info.may_return_rows = false;
            info.tag = "SECURITY LABEL".into();
        }
        "IMPORT" => {
            info.may_return_rows = false;
            info.tag = "IMPORT FOREIGN SCHEMA".into();
        }
        "REASSIGN" => {
            info.may_return_rows = false;
            info.tag = "REASSIGN OWNED".into();
        }
        "DEALLOCATE" => {
            info.may_return_rows = false;
            info.tag = if words.iter().skip(1).take(2).any(|x| x == "ALL") { "DEALLOCATE ALL".into() } else { "DEALLOCATE".into() };
        }
        v if NO_ROW_VERBS.contains(&v) => {
            info.may_return_rows = false;
        }
        _ => {}
    }
    if info.tx != TxEffect::None {
        info.may_return_rows = false;
    }
    info
}

/// `CREATE TABLE`, `ALTER MATERIALIZED VIEW`, `DROP FOREIGN TABLE` ...
fn ddl_tag(words: &[String]) -> String {
    let verb = words.first().map(String::as_str).unwrap_or("");
    let mut rest = words.iter().skip(1).map(String::as_str).filter(|w| !TAG_MODIFIERS.contains(w)).peekable();
    let Some(first) = rest.next() else { return verb.to_string() };
    let second = rest.next().unwrap_or("");
    let third = rest.next().unwrap_or("");
    let obj = match (first, second) {
        ("MATERIALIZED", "VIEW") => "MATERIALIZED VIEW".to_string(),
        ("FOREIGN", "TABLE") => "FOREIGN TABLE".to_string(),
        ("FOREIGN", "DATA") => "FOREIGN DATA WRAPPER".to_string(),
        ("EVENT", "TRIGGER") => "EVENT TRIGGER".to_string(),
        ("USER", "MAPPING") => "USER MAPPING".to_string(),
        ("ACCESS", "METHOD") => "ACCESS METHOD".to_string(),
        ("DEFAULT", "PRIVILEGES") => "DEFAULT PRIVILEGES".to_string(),
        ("OPERATOR", "CLASS") => "OPERATOR CLASS".to_string(),
        ("OPERATOR", "FAMILY") => "OPERATOR FAMILY".to_string(),
        ("TEXT", "SEARCH") => format!("TEXT SEARCH {third}"),
        ("DEFAULT", _) => "DEFAULT PRIVILEGES".to_string(),
        (f, _) => f.to_string(),
    };
    format!("{verb} {obj}")
}

fn contains_seq(words: &[String], seq: &[&str]) -> bool {
    words.windows(seq.len()).any(|w| w.iter().zip(seq).all(|(a, b)| a == b))
}

fn contains_seq_str(words: &[&str], seq: &[&str]) -> bool {
    words.windows(seq.len()).any(|w| w.iter().zip(seq).all(|(a, b)| a == b))
}

/// Analysis of a complete text sent in one `execute` call.
#[derive(Debug, Clone)]
pub(crate) struct ScriptInfo {
    pub statements: Vec<StmtInfo>,
}

impl ScriptInfo {
    pub(crate) fn analyze(sql: &str) -> ScriptInfo {
        let statements = split_statements(sql).into_iter().map(classify).collect();
        ScriptInfo { statements }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.statements.is_empty()
    }

    pub(crate) fn first(&self) -> Option<&StmtInfo> {
        self.statements.first()
    }

    pub(crate) fn has_copy_from_stdin(&self) -> bool {
        self.statements.iter().any(|s| s.copy == CopyKind::FromStdin)
    }
}

/// Remove a trailing `;` (and whitespace) from a statement.
pub(crate) fn strip_semicolon(sql: &str) -> &str {
    let mut s = sql.trim_end();
    while let Some(rest) = s.strip_suffix(';') {
        s = rest.trim_end();
    }
    s
}

/// Byte offset just after the `ON <qualified name>` target of a
/// `CREATE INDEX` / `CREATE TRIGGER` definition, together with the start of
/// the name. Returns `(name_start, name_end)`.
pub(crate) fn find_on_target(def: &str) -> Option<(usize, usize)> {
    let toks = tokenize(def);
    let mut i = 0usize;
    while i < toks.len() {
        if let Tok::Word(w) = toks[i].tok
            && w.eq_ignore_ascii_case("on")
        {
            let mut j = i + 1;
            if let Some(Tok::Word(o)) = toks.get(j).map(|t| t.tok)
                && o.eq_ignore_ascii_case("only")
            {
                j += 1;
            }
            let start = toks.get(j)?.start;
            let mut end = toks.get(j)?.end;
            if !matches!(toks[j].tok, Tok::Word(_) | Tok::Quoted(_)) {
                return None;
            }
            // optional .name
            if toks.get(j + 1).map(|t| t.tok) == Some(Tok::Dot)
                && let Some(n) = toks.get(j + 2)
                && matches!(n.tok, Tok::Word(_) | Tok::Quoted(_))
            {
                end = n.end;
            }
            return Some((start, end));
        }
        i += 1;
    }
    None
}

/// Remove `ONLY` from `... ON ONLY table ...` (definitions of partitioned
/// indexes): replaying the statement then creates the index on every partition.
pub(crate) fn strip_on_only(def: &str) -> String {
    let toks = tokenize(def);
    for (i, t) in toks.iter().enumerate() {
        if let Tok::Word(w) = t.tok
            && w.eq_ignore_ascii_case("on")
        {
            if let (Some(only), Some(next)) = (toks.get(i + 1), toks.get(i + 2))
                && matches!(only.tok, Tok::Word(o) if o.eq_ignore_ascii_case("only"))
            {
                let mut out = String::with_capacity(def.len());
                out.push_str(&def[..only.start]);
                out.push_str(&def[next.start..]);
                return out;
            }
            break;
        }
    }
    def.to_string()
}

/// Replace the table referenced by `ON ...` in an index / trigger definition.
pub(crate) fn replace_on_target(def: &str, new_target: &str) -> Option<String> {
    let (s, e) = find_on_target(def)?;
    let mut out = String::with_capacity(def.len() + new_target.len());
    out.push_str(&def[..s]);
    out.push_str(new_target);
    out.push_str(&def[e..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_quotes_and_comments() {
        let t = tokenize("select 'a;b', \"x;y\", $$c;d$$, $f$e;$f$, E'it\\'s;' -- c;\n /* a /* b; */ ; */ $1;");
        let semis = t.iter().filter(|t| t.tok == Tok::Semi).count();
        assert_eq!(semis, 1);
        assert!(t.iter().any(|t| t.tok == Tok::Param));
        assert_eq!(t.iter().filter(|t| t.tok == Tok::Str).count(), 4);
    }

    #[test]
    fn splits_statements() {
        let s = split_statements("select 1; select ';'; ; -- x\n insert into t values (1);");
        assert_eq!(s, vec!["select 1", "select ';'", "insert into t values (1)"]);
        let s = split_statements("create rule r as on insert to t do also (insert into a values(1); insert into b values(2)); select 2");
        assert_eq!(s.len(), 2);
        let f = "create function f() returns int language sql begin atomic select 1; select 2; end; select 3;";
        let s = split_statements(f);
        assert_eq!(s.len(), 2, "{s:?}");
        assert!(s[0].ends_with("end"));
        let p = "create or replace procedure p() language sql begin atomic insert into t values (case when true then 1 else 2 end); end;";
        assert_eq!(split_statements(p).len(), 1);
        assert_eq!(split_statements("do $$ begin perform 1; end $$; select 1").len(), 2);
        assert!(split_statements(" -- only comment\n").is_empty());
    }

    #[test]
    fn transaction_keywords() {
        assert_eq!(classify("BEGIN").tx, TxEffect::Begin);
        assert_eq!(classify("begin isolation level serializable").tx, TxEffect::Begin);
        assert_eq!(classify("start transaction read only").tx, TxEffect::Begin);
        assert_eq!(classify("commit").tx, TxEffect::Commit { chain: false });
        assert_eq!(classify("end work").tx, TxEffect::Commit { chain: false });
        assert_eq!(classify("commit and chain").tx, TxEffect::Commit { chain: true });
        assert_eq!(classify("commit and no chain").tx, TxEffect::Commit { chain: false });
        assert_eq!(classify("abort").tx, TxEffect::Rollback { chain: false });
        assert_eq!(classify("rollback to savepoint a").tx, TxEffect::Savepoint);
        assert_eq!(classify("rollback work to a").tx, TxEffect::Savepoint);
        assert_eq!(classify("savepoint a").tx, TxEffect::Savepoint);
        assert_eq!(classify("release savepoint a").tx, TxEffect::Savepoint);
        assert_eq!(classify("prepare transaction 'x'").tx, TxEffect::PrepareTransaction);
        let cp = classify("commit prepared 'x'");
        assert_eq!(cp.tx, TxEffect::FinishPrepared);
        assert!(cp.needs_autocommit);
        assert_eq!(classify("prepare q as select 1").tx, TxEffect::None);
        assert!(!classify("savepoint a").tx.is_transaction_control());
        assert!(classify("commit").tx.is_transaction_control());
    }

    #[test]
    fn rows_and_tags() {
        let i = classify("insert into t values (1)");
        assert!(!i.may_return_rows && i.counts_rows);
        assert_eq!(i.command_tag(3), "INSERT 0 3");
        assert_eq!(i.rows_affected(3), Some(3));
        assert!(classify("insert into t values (1) returning id").may_return_rows);
        assert!(classify("with x as (delete from t returning *) select * from x").may_return_rows);
        let w = classify("with x as (select 1) update t set a = 1 from x");
        assert_eq!(w.main_verb, "UPDATE");
        assert!(!w.may_return_rows);
        assert_eq!(w.command_tag(2), "UPDATE 2");
        let c = classify("create unlogged table if not exists s.t (a int generated always as (1) stored)");
        assert_eq!(c.tag, "CREATE TABLE");
        assert_eq!(c.rows_affected(0), None);
        let ctas = classify("create table t2 as select * from t");
        assert_eq!(ctas.command_tag(5), "SELECT 5");
        assert_eq!(classify("create or replace view v as select 1").tag, "CREATE VIEW");
        assert_eq!(classify("create materialized view mv as select 1").command_tag(1), "SELECT 1");
        assert_eq!(classify("drop materialized view mv").tag, "DROP MATERIALIZED VIEW");
        assert_eq!(classify("alter table t add column b int").tag, "ALTER TABLE");
        assert_eq!(classify("create unique index concurrently i on t(a)").tag, "CREATE INDEX");
        assert_eq!(classify("create text search configuration c (copy = english)").tag, "CREATE TEXT SEARCH CONFIGURATION");
        assert_eq!(classify("truncate t").tag, "TRUNCATE TABLE");
        assert!(classify("select 1").may_return_rows);
        assert!(classify("(select 1) union (select 2)").may_return_rows);
        assert_eq!(classify("(select 1)").verb, "SELECT");
        assert!(classify("show search_path").may_return_rows);
        assert!(!classify("set search_path = public").may_return_rows);
        assert!(classify("explain select 1").may_return_rows);
        assert!(classify("call p(1)").may_return_rows);
        assert!(!classify("select * into newtab from t").may_return_rows);
        assert!(classify("select $1::int").has_params);
    }

    #[test]
    fn autocommit_only_statements() {
        for s in [
            "vacuum full t",
            "create database x",
            "drop database if exists x",
            "create index concurrently i on t(a)",
            "drop index concurrently i",
            "reindex table concurrently t",
            "reindex database x",
            "alter system set work_mem = '1MB'",
            "cluster",
            "cluster verbose",
            "discard all",
            "alter table p detach partition c concurrently",
        ] {
            assert!(classify(s).needs_autocommit, "{s}");
        }
        for s in ["cluster t", "reindex table t", "create index i on t(a)", "analyze t", "select 1"] {
            assert!(!classify(s).needs_autocommit, "{s}");
        }
    }

    #[test]
    fn copy_detection() {
        let s = ScriptInfo::analyze("select 1; copy t from stdin;");
        assert!(s.has_copy_from_stdin());
        assert_eq!(classify("COPY (select 1) TO STDOUT WITH CSV").copy, CopyKind::ToStdout);
        assert_eq!(classify("copy t to '/tmp/x'").copy, CopyKind::None);
    }

    #[test]
    fn on_target_replacement() {
        let d = "CREATE INDEX \"idx on\" ON ONLY s1.\"T\" USING btree (a) WHERE (b > 0)";
        assert_eq!(replace_on_target(d, "s2.\"T\"").unwrap(), "CREATE INDEX \"idx on\" ON ONLY s2.\"T\" USING btree (a) WHERE (b > 0)");
        let t = "CREATE TRIGGER trg BEFORE INSERT OR UPDATE OF a ON s1.t FOR EACH ROW EXECUTE FUNCTION s1.f()";
        assert_eq!(replace_on_target(t, "x.y").unwrap(), "CREATE TRIGGER trg BEFORE INSERT OR UPDATE OF a ON x.y FOR EACH ROW EXECUTE FUNCTION s1.f()");
        assert_eq!(replace_on_target("CREATE INDEX i ON t (a)", "s.t").unwrap(), "CREATE INDEX i ON s.t (a)");
        assert_eq!(strip_on_only("CREATE INDEX i ON ONLY s.p USING gin (x)"), "CREATE INDEX i ON s.p USING gin (x)");
        assert_eq!(strip_on_only("CREATE INDEX \"only\" ON s.p (x)"), "CREATE INDEX \"only\" ON s.p (x)");
    }
}
