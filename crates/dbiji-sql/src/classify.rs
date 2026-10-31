//! Statement classification: what a statement does, whether it writes,
//! whether it returns rows, whether it is dangerous and what it targets.
//!
//! This drives read-only enforcement (FR-C6: every statement with
//! [`StatementInfo::is_write`] is rejected on read-only connections) and the
//! confirmations for dangerous statements on production connections, so the
//! rules are conservative: when in doubt a statement is a write.

use crate::split::{SplitOptions, StatementSpan, split_tokens};
use crate::token::{Token, TokenKind, is_word, tokenize, unquote_ident};
use dbiji_core::SqlSyntax;
use serde::{Deserialize, Serialize};

/// Transaction control statements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TxCommand {
    /// `BEGIN`, `START TRANSACTION`
    Begin,
    /// `COMMIT`, `END` (PostgreSQL)
    Commit,
    /// `ROLLBACK [TO SAVEPOINT x]`, `ABORT`
    Rollback,
    /// `SAVEPOINT x`
    Savepoint,
    /// `RELEASE [SAVEPOINT] x`
    Release,
    /// `SET TRANSACTION ...`, `SET SESSION CHARACTERISTICS AS TRANSACTION ...`
    SetTransaction,
}

/// What a statement does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StatementKind {
    /// Queries: `SELECT`, `WITH ... SELECT`, `VALUES`, `TABLE x`.
    Select,
    /// `INSERT`, `REPLACE INTO`, `UPSERT`.
    Insert,
    /// `UPDATE`
    Update,
    /// `DELETE`
    Delete,
    /// `MERGE`
    Merge,
    /// `CREATE ...`
    Create,
    /// `ALTER ...`
    Alter,
    /// `DROP ...`
    Drop,
    /// `TRUNCATE`
    Truncate,
    /// `RENAME` (MySQL)
    Rename,
    /// `COMMENT ON ...`
    Comment,
    /// `GRANT`
    Grant,
    /// `REVOKE`
    Revoke,
    /// Transaction control.
    Transaction(TxCommand),
    /// Session settings: `SET`, `RESET`.
    Set,
    /// `SHOW ...`
    Show,
    /// `USE db`
    Use,
    /// `CALL`, `EXEC`, `EXECUTE`, `DO`.
    Call,
    /// `EXPLAIN [ANALYZE] ...`
    Explain,
    /// `DESCRIBE` / `DESC`
    Describe,
    /// `COPY`, `LOAD DATA`
    Copy,
    /// `VACUUM`, `ANALYZE`, `REINDEX`, `OPTIMIZE`, `CHECK`, `REPAIR`, `CLUSTER` ...
    Maintenance,
    /// SQLite `PRAGMA`
    Pragma,
    /// `LOCK` / `UNLOCK`
    Lock,
    /// Client side command (`@set`, `@include` ...), handled by the application.
    ClientCommand,
    /// Anything else (treated as a write).
    Other,
}

impl StatementKind {
    /// Short English label (route through `tr()` for display).
    pub fn label(self) -> &'static str {
        match self {
            StatementKind::Select => "Query",
            StatementKind::Insert => "Insert",
            StatementKind::Update => "Update",
            StatementKind::Delete => "Delete",
            StatementKind::Merge => "Merge",
            StatementKind::Create => "Create",
            StatementKind::Alter => "Alter",
            StatementKind::Drop => "Drop",
            StatementKind::Truncate => "Truncate",
            StatementKind::Rename => "Rename",
            StatementKind::Comment => "Comment",
            StatementKind::Grant => "Grant",
            StatementKind::Revoke => "Revoke",
            StatementKind::Transaction(_) => "Transaction",
            StatementKind::Set => "Set",
            StatementKind::Show => "Show",
            StatementKind::Use => "Use",
            StatementKind::Call => "Call",
            StatementKind::Explain => "Explain",
            StatementKind::Describe => "Describe",
            StatementKind::Copy => "Copy",
            StatementKind::Maintenance => "Maintenance",
            StatementKind::Pragma => "Pragma",
            StatementKind::Lock => "Lock",
            StatementKind::ClientCommand => "Client command",
            StatementKind::Other => "Statement",
        }
    }
}

/// Reasons a statement needs confirmation on production connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Danger {
    /// `DELETE` without a top-level `WHERE`.
    DeleteWithoutWhere,
    /// `UPDATE` without a top-level `WHERE`.
    UpdateWithoutWhere,
    /// Any `DROP`.
    Drop,
    /// `TRUNCATE`.
    Truncate,
    /// `ALTER ... DROP` (columns, constraints, partitions ...).
    AlterDrop,
    /// Redis `FLUSHDB` / `FLUSHALL`: every key of a database goes.
    Flush,
    /// A command that stops or reconfigures the server (`SHUTDOWN`, `DEBUG`).
    ServerCommand,
}

impl Danger {
    /// English description (route through `tr()` for display).
    pub fn message(self) -> &'static str {
        match self {
            Danger::DeleteWithoutWhere => "DELETE without WHERE removes every row",
            Danger::UpdateWithoutWhere => "UPDATE without WHERE changes every row",
            Danger::Drop => "DROP permanently removes the object",
            Danger::Truncate => "TRUNCATE removes every row",
            Danger::AlterDrop => "ALTER ... DROP removes part of the object",
            Danger::Flush => "FLUSH removes every key of the database",
            Danger::ServerCommand => "The command stops or reconfigures the server",
        }
    }
}

/// Possibly qualified object name, unquoted.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct QualifiedName {
    /// Database / catalog of three-part names (`db.schema.table`).
    pub catalog: Option<String>,
    /// Schema part, if written.
    pub schema: Option<String>,
    /// Object name.
    pub name: String,
}

/// Classification result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatementInfo {
    /// What the statement does.
    pub kind: StatementKind,
    /// First keyword, upper case (`SELECT`, `WITH`, `@SET` ...); empty for
    /// statements that do not start with a word.
    pub first_keyword: String,
    /// The statement probably produces a result set.
    pub returns_rows: bool,
    /// The statement may modify data, schema, server state or files.
    pub is_write: bool,
    /// Why the statement needs a confirmation, if it does.
    pub danger: Option<Danger>,
    /// Main object: the table written to, the object created/altered/dropped,
    /// the first table read by a query, the procedure called.
    pub target: Option<QualifiedName>,
}

impl StatementInfo {
    fn new(kind: StatementKind, first_keyword: String) -> StatementInfo {
        StatementInfo {
            kind,
            first_keyword,
            returns_rows: false,
            is_write: false,
            danger: None,
            target: None,
        }
    }

    fn rows(mut self) -> Self {
        self.returns_rows = true;
        self
    }

    fn write(mut self) -> Self {
        self.is_write = true;
        self
    }

    /// Transaction control (`BEGIN`, `COMMIT`, `ROLLBACK`, ...).
    pub fn is_transaction_control(&self) -> bool {
        matches!(self.kind, StatementKind::Transaction(_))
    }
}

/// Classify one statement.
pub fn classify(stmt: &str, syntax: &SqlSyntax) -> StatementInfo {
    if let Some(shell) = syntax.shell {
        return crate::shell::classify_shell(stmt, shell);
    }
    let tokens = tokenize(stmt, syntax);
    classify_tokens(stmt, &tokens, syntax)
}

/// Split a script and classify every statement (tokenizes once).
pub fn split_and_classify(
    src: &str,
    syntax: &SqlSyntax,
    opts: &SplitOptions,
) -> Vec<(StatementSpan, StatementInfo)> {
    if let Some(shell) = syntax.shell {
        return crate::shell::split_shell(src)
            .into_iter()
            .map(|s| (s, crate::shell::classify_shell(s.text(src), shell)))
            .collect();
    }
    let split = split_tokens(src, syntax, opts);
    split
        .statements
        .iter()
        .map(|s| {
            let toks = split.tokens.get(s.tokens.clone()).unwrap_or(&[]);
            (s.span, classify_tokens(src, toks, syntax))
        })
        .collect()
}

/// Classify the statement made of `tokens` (offsets into `src`).
pub(crate) fn classify_tokens(src: &str, tokens: &[Token], syntax: &SqlSyntax) -> StatementInfo {
    let sig: Vec<Token> = tokens.iter().filter(|t| !t.is_trivia()).copied().collect();
    if sig.is_empty() {
        if let Some(c) = tokens
            .iter()
            .find(|t| t.kind == TokenKind::BlockComment && t.text(src).starts_with("/*!"))
        {
            let inner = exec_comment_body(c.text(src));
            let toks = tokenize(inner, syntax);
            let sig: Vec<Token> = toks.iter().filter(|t| !t.is_trivia()).copied().collect();
            return Sig {
                src: inner,
                t: &sig,
            }
            .classify(0, 0);
        }
        return StatementInfo::new(StatementKind::Other, String::new());
    }
    let mut info = Sig { src, t: &sig }.classify(0, 0);
    // T-SQL runs statements one after another without `;` (`SELECT 1 DELETE FROM t`,
    // `UPDATE t SET a = 1 DELETE FROM u WHERE id = 5`): one span may hold several.
    // Each is classified on its own tokens, so a later WHERE does not count for an
    // earlier UPDATE.
    if syntax.batch_separator.is_some() {
        let (mut from, mut kind, mut is_write) = (0, info.kind, info.is_write);
        let mut several = false;
        let mut danger = None;
        loop {
            let next = next_batch_statement(src, &sig, from, kind, is_write);
            if next.is_none() && !several {
                break;
            }
            let piece = Sig {
                src,
                t: &sig[from..next.unwrap_or(sig.len())],
            }
            .classify(0, 0);
            info.is_write |= piece.is_write;
            danger = danger.or(piece.danger);
            several = true;
            let Some(next) = next else { break };
            // What the next statement is shows in its first words.
            let head = Sig {
                src,
                t: &sig[next..(next + 16).min(sig.len())],
            }
            .classify(0, 0);
            (from, kind, is_write) = (next, head.kind, head.is_write);
        }
        if several {
            info.danger = danger;
        }
    }
    // MySQL runs `/*! … */` comments: `SELECT * FROM t /*!INTO OUTFILE '/tmp/x'*/` writes.
    if syntax.delimiter_command && !info.is_write {
        for c in tokens
            .iter()
            .filter(|t| t.kind == TokenKind::BlockComment && t.text(src).starts_with("/*!"))
        {
            let inner = exec_comment_body(c.text(src));
            let toks = tokenize(inner, syntax);
            let sig: Vec<Token> = toks.iter().filter(|t| !t.is_trivia()).copied().collect();
            let words: Vec<String> = sig
                .iter()
                .map(|t| t.text(inner).to_ascii_uppercase())
                .collect();
            let into_file = words
                .windows(2)
                .any(|w| w[0] == "INTO" && (w[1] == "OUTFILE" || w[1] == "DUMPFILE"));
            // A fragment (`, 2`, `PARTITION BY …`) is no statement: only a writing statement word counts.
            let writes = words.first().is_some_and(|w| {
                LATER_WRITE_STARTS.contains(&w.as_str())
                    || ["REPLACE", "RENAME", "LOAD"].contains(&w.as_str())
            });
            if into_file || writes {
                info.is_write = true;
                break;
            }
        }
    }
    info
}

/// Statements that may follow another one in a T-SQL batch without `;`.
/// Start of the T-SQL statement after the one at `from` (of `kind`), in a
/// run without `;`. Statements that hold others (MERGE's actions, CREATE's
/// body, GRANT's privileges) are not cut.
fn next_batch_statement(
    src: &str,
    sig: &[Token],
    from: usize,
    kind: StatementKind,
    is_write: bool,
) -> Option<usize> {
    use StatementKind as K;
    let cut = !is_write
        || matches!(
            kind,
            K::Insert | K::Update | K::Delete | K::Drop | K::Truncate | K::Call
        );
    if !cut {
        return None;
    }
    let mut depth = 0i32;
    for (k, t) in sig.iter().enumerate().skip(from + 1) {
        match t.kind {
            TokenKind::LParen => depth += 1,
            TokenKind::RParen => depth -= 1,
            _ if depth == 0 => {
                let w = t.text(src);
                let is = |s: &str| w.eq_ignore_ascii_case(s);
                let write = LATER_WRITE_STARTS.iter().any(|s| is(s))
                    // `a INNER MERGE JOIN b` is a join hint, no MERGE statement.
                    && !(is("MERGE") && sig.get(k + 1).is_some_and(|n| n.text(src).eq_ignore_ascii_case("JOIN")))
                    // `INSERT … EXEC p` fills the insert.
                    && !(kind == K::Insert && (is("EXEC") || is("EXECUTE")));
                // After UPDATE / DELETE a SELECT is the next statement (`INSERT … SELECT` fills the insert).
                let read = is("SELECT") && matches!(kind, K::Update | K::Delete);
                if write || read {
                    return Some(k);
                }
            }
            _ => {}
        }
    }
    None
}

const LATER_WRITE_STARTS: &[&str] = &[
    "INSERT",
    "UPDATE",
    "DELETE",
    "MERGE",
    "DROP",
    "ALTER",
    "CREATE",
    "TRUNCATE",
    "EXEC",
    "EXECUTE",
    "GRANT",
    "REVOKE",
    "DENY",
    "BACKUP",
    "RESTORE",
    "DBCC",
    "BULK",
    "RECONFIGURE",
    "KILL",
    "SHUTDOWN",
];

/// Body of a MySQL executable comment: `/*!40101 SET x */` → `SET x`.
fn exec_comment_body(text: &str) -> &str {
    let inner = text.strip_prefix("/*!").unwrap_or(text);
    let inner = inner.strip_suffix("*/").unwrap_or(inner);
    inner
        .trim_start_matches(|c: char| c.is_ascii_digit())
        .trim()
}

const MAX_DEPTH: u32 = 8;

const QUERY_PRAGMAS: &[&str] = &[
    "table_info",
    "table_xinfo",
    "table_list",
    "index_list",
    "index_info",
    "index_xinfo",
    "foreign_key_list",
    "foreign_key_check",
    "integrity_check",
    "quick_check",
    "database_list",
    "collation_list",
    "function_list",
    "module_list",
    "pragma_list",
    "compile_options",
    "page_count",
    "freelist_count",
    "data_version",
];

struct Sig<'a> {
    src: &'a str,
    t: &'a [Token],
}

impl<'a> Sig<'a> {
    fn sub(&self, from: usize, to: usize) -> Sig<'a> {
        let to = to.min(self.t.len());
        let from = from.min(to);
        Sig {
            src: self.src,
            t: &self.t[from..to],
        }
    }

    fn kind(&self, i: usize) -> Option<TokenKind> {
        self.t.get(i).map(|t| t.kind)
    }

    fn w(&self, i: usize, word: &str) -> bool {
        self.t.get(i).is_some_and(|t| is_word(t, self.src, word))
    }

    fn any(&self, i: usize, words: &[&str]) -> bool {
        words.iter().any(|w| self.w(i, w))
    }

    fn text(&self, i: usize) -> &'a str {
        self.t.get(i).map(|t| t.text(self.src)).unwrap_or("")
    }

    fn upper(&self, i: usize) -> String {
        match self.kind(i) {
            Some(TokenKind::Keyword | TokenKind::Identifier) => self.text(i).to_ascii_uppercase(),
            _ => String::new(),
        }
    }

    /// Index of the `)` matching the `(` at `i`.
    fn close_paren(&self, i: usize) -> usize {
        let mut depth = 0i32;
        for j in i..self.t.len() {
            match self.t[j].kind {
                TokenKind::LParen => depth += 1,
                TokenKind::RParen => {
                    depth -= 1;
                    if depth <= 0 {
                        return j;
                    }
                }
                _ => {}
            }
        }
        self.t.len()
    }

    /// First index >= `from` at parenthesis depth 0 satisfying `pred`.
    fn find_top(&self, from: usize, pred: impl Fn(usize) -> bool) -> Option<usize> {
        let mut depth = 0i32;
        for j in from..self.t.len() {
            match self.t[j].kind {
                TokenKind::LParen => depth += 1,
                TokenKind::RParen => depth -= 1,
                _ if depth <= 0 && pred(j) => return Some(j),
                _ => {}
            }
        }
        None
    }

    fn has_top_word(&self, from: usize, word: &str) -> bool {
        self.find_top(from, |j| self.w(j, word)).is_some()
    }

    fn ident_text(&self, i: usize) -> Option<String> {
        let t = self.t.get(i)?;
        match t.kind {
            TokenKind::Identifier | TokenKind::Keyword => Some(t.text(self.src).to_string()),
            TokenKind::QuotedIdentifier => Some(unquote_ident(t.text(self.src)).into_owned()),
            _ => None,
        }
    }

    /// Dotted name starting at `i`.
    fn name_at(&self, i: usize) -> Option<QualifiedName> {
        let mut parts = vec![self.ident_text(i)?];
        let mut j = i + 1;
        while self.kind(j) == Some(TokenKind::Dot) && parts.len() < 4 {
            match self.ident_text(j + 1) {
                Some(p) => parts.push(p),
                None => break,
            }
            j += 2;
        }
        let name = parts.pop()?;
        let schema = parts.pop();
        let catalog = parts.pop();
        Some(QualifiedName {
            catalog,
            schema,
            name,
        })
    }

    /// Skip words in `skip` starting at `i`.
    fn skip_words(&self, mut i: usize, skip: &[&str]) -> usize {
        while self.any(i, skip) {
            i += 1;
        }
        i
    }

    fn skip_if_exists(&self, mut i: usize) -> usize {
        if self.w(i, "IF") {
            i += 1;
            if self.w(i, "NOT") {
                i += 1;
            }
            if self.w(i, "EXISTS") {
                i += 1;
            }
        }
        i
    }

    fn top_from_target(&self, from: usize) -> Option<QualifiedName> {
        let f = self.find_top(from, |j| self.w(j, "FROM"))?;
        let i = self.skip_words(f + 1, &["ONLY", "LATERAL"]);
        if self.kind(i) == Some(TokenKind::LParen) {
            return None;
        }
        self.name_at(i)
    }

    fn returning(&self, from: usize) -> bool {
        self.find_top(from, |j| self.w(j, "RETURNING") || self.w(j, "OUTPUT"))
            .is_some()
    }

    fn classify(&self, start: usize, depth: u32) -> StatementInfo {
        let mut p = start;
        while self.kind(p) == Some(TokenKind::LParen) {
            p += 1;
        }
        let first = self.upper(p);
        if self.kind(p) == Some(TokenKind::ClientCommand) {
            let text = self.text(p);
            let name: String = text.chars().take_while(|c| !c.is_whitespace()).collect();
            return StatementInfo::new(StatementKind::ClientCommand, name.to_uppercase());
        }
        if depth > MAX_DEPTH {
            return StatementInfo::new(StatementKind::Other, first).write();
        }
        use StatementKind as K;
        let info = StatementInfo::new;
        match first.as_str() {
            "SELECT" => self.select(p, first),
            "WITH" => self.with(p, depth),
            "VALUES" | "SUMMARIZE" => info(K::Select, first).rows(),
            "TABLE" => {
                let mut i = info(K::Select, first).rows();
                i.target = self.name_at(p + 1);
                i
            }
            "INSERT" | "REPLACE" | "UPSERT" => {
                let mut i = info(K::Insert, first).write();
                i.returns_rows = self.returning(p);
                let into = (p + 1..(p + 6).min(self.t.len())).find(|&j| self.w(j, "INTO"));
                i.target = match into {
                    Some(j) => self.name_at(j + 1),
                    None => self.name_at(self.skip_words(
                        p + 1,
                        &["LOW_PRIORITY", "DELAYED", "HIGH_PRIORITY", "IGNORE"],
                    )),
                };
                i
            }
            "UPDATE" => {
                let mut i = info(K::Update, first).write();
                if !self.has_top_word(p + 1, "WHERE") {
                    i.danger = Some(Danger::UpdateWithoutWhere);
                }
                i.returns_rows = self.returning(p);
                let mut j = self.skip_words(p + 1, &["ONLY", "LOW_PRIORITY", "IGNORE"]);
                if self.w(j, "OR") {
                    j += 2;
                }
                i.target = self.name_at(j);
                i
            }
            "DELETE" => {
                let mut i = info(K::Delete, first).write();
                if !self.has_top_word(p + 1, "WHERE") {
                    i.danger = Some(Danger::DeleteWithoutWhere);
                }
                i.returns_rows = self.returning(p);
                i.target = self.top_from_target(p + 1).or_else(|| self.name_at(p + 1));
                i
            }
            "MERGE" => {
                let mut i = info(K::Merge, first).write();
                i.returns_rows = self.returning(p);
                let j = if self.w(p + 1, "INTO") { p + 2 } else { p + 1 };
                i.target = self.name_at(j);
                i
            }
            "CREATE" => {
                let mut i = info(K::Create, first).write();
                i.target = self.object_name_after_kind(p + 1);
                i
            }
            "ALTER" => {
                let mut i = info(K::Alter, first).write();
                i.target = self.object_name_after_kind(p + 1);
                let drop = self.find_top(p + 1, |j| {
                    self.w(j, "DROP")
                        && !self.any(j + 1, &["DEFAULT", "NOT", "IDENTITY", "EXPRESSION"])
                });
                if drop.is_some() {
                    i.danger = Some(Danger::AlterDrop);
                }
                i
            }
            "DROP" => {
                let mut i = info(K::Drop, first).write();
                i.danger = Some(Danger::Drop);
                i.target = self.object_name_after_kind(p + 1);
                i
            }
            "TRUNCATE" => {
                let mut i = info(K::Truncate, first).write();
                i.danger = Some(Danger::Truncate);
                i.target = self.name_at(self.skip_words(p + 1, &["TABLE", "ONLY"]));
                i
            }
            "RENAME" => {
                let mut i = info(K::Rename, first).write();
                i.target = self.name_at(self.skip_words(p + 1, &["TABLE", "USER"]));
                i
            }
            "COMMENT" => info(K::Comment, first).write(),
            "GRANT" => info(K::Grant, first).write(),
            "REVOKE" => info(K::Revoke, first).write(),
            "BEGIN" => {
                let next = p + 1;
                let tx = next >= self.t.len()
                    || self.any(
                        next,
                        &[
                            "TRANSACTION",
                            "TRAN",
                            "WORK",
                            "DEFERRED",
                            "IMMEDIATE",
                            "EXCLUSIVE",
                            "ISOLATION",
                            "READ",
                            "DISTRIBUTED",
                        ],
                    );
                if tx {
                    let mut i = info(K::Transaction(TxCommand::Begin), first);
                    i.is_write = self.read_write(next);
                    i
                } else {
                    info(K::Other, first).write()
                }
            }
            "START" => {
                if self.w(p + 1, "TRANSACTION") {
                    let mut i = info(K::Transaction(TxCommand::Begin), first);
                    i.is_write = self.read_write(p + 2);
                    i
                } else {
                    info(K::Other, first).write()
                }
            }
            "COMMIT" | "END" => info(K::Transaction(TxCommand::Commit), first),
            "ROLLBACK" | "ABORT" => info(K::Transaction(TxCommand::Rollback), first),
            "SAVEPOINT" => info(K::Transaction(TxCommand::Savepoint), first),
            "RELEASE" => info(K::Transaction(TxCommand::Release), first),
            "SET" => self.set(p, first),
            "RESET" => {
                if self.any(
                    p + 1,
                    &["MASTER", "SLAVE", "REPLICA", "QUERY", "BINARY", "PERSIST"],
                ) {
                    info(K::Set, first).write()
                } else {
                    info(K::Set, first)
                }
            }
            "SHOW" => info(K::Show, first).rows(),
            "DESCRIBE" | "DESC" => {
                let mut i = info(K::Describe, first).rows();
                i.target = self.name_at(p + 1);
                i
            }
            "EXPLAIN" => self.explain(p, first, depth),
            "USE" => info(K::Use, first),
            "CALL" | "EXEC" | "EXECUTE" => {
                let mut i = info(K::Call, first).write().rows();
                i.target = self.name_at(p + 1);
                i
            }
            "DO" => info(K::Call, first).write(),
            "COPY" => {
                let mut i = info(K::Copy, first);
                let to_stdout = self
                    .find_top(p + 1, |j| self.w(j, "TO"))
                    .is_some_and(|j| self.any(j + 1, &["STDOUT"]));
                i.is_write = !to_stdout;
                if self.kind(p + 1) != Some(TokenKind::LParen) {
                    i.target = self.name_at(p + 1);
                }
                i
            }
            "LOAD" | "IMPORT" => info(K::Copy, first).write(),
            "VACUUM" | "ANALYZE" | "ANALYSE" | "REINDEX" | "CLUSTER" | "CHECKPOINT" | "REFRESH"
            | "FLUSH" => info(K::Maintenance, first).write(),
            "OPTIMIZE" | "CHECK" | "REPAIR" | "CHECKSUM" => {
                info(K::Maintenance, first).write().rows()
            }
            "PRAGMA" => self.pragma(p, first),
            "LOCK" => {
                let mut i = info(K::Lock, first).write();
                i.target = self.name_at(self.skip_words(p + 1, &["TABLE", "TABLES"]));
                i
            }
            "UNLOCK" => info(K::Lock, first),
            "PREPARE" => match self.find_top(p + 1, |j| self.w(j, "AS")) {
                Some(a) if !self.w(p + 1, "TRANSACTION") => {
                    let inner = self.classify(a + 1, depth + 1);
                    let mut i = info(K::Other, first);
                    i.is_write = inner.is_write;
                    i
                }
                _ => info(K::Other, first).write(),
            },
            "DECLARE" => {
                if self.kind(p + 1) == Some(TokenKind::Variable) {
                    return info(K::Other, first);
                }
                match self.find_top(p + 1, |j| self.w(j, "FOR")) {
                    Some(f) if self.has_top_word(p + 1, "CURSOR") => {
                        let inner = self.classify(f + 1, depth + 1);
                        let mut i = info(K::Other, first);
                        i.is_write = inner.is_write;
                        i
                    }
                    _ => info(K::Other, first).write(),
                }
            }
            "FETCH" | "MOVE" => info(K::Other, first).rows(),
            "CLOSE" | "DEALLOCATE" | "DISCARD" | "LISTEN" | "UNLISTEN" | "PRINT" | "WAITFOR"
            | "RAISERROR" | "THROW" => info(K::Other, first),
            _ => info(K::Other, first).write(),
        }
    }

    fn select(&self, p: usize, first: String) -> StatementInfo {
        let mut i = StatementInfo::new(StatementKind::Select, first).rows();
        if let Some(into) = self.find_top(p + 1, |j| self.w(j, "INTO")) {
            i.returns_rows = false;
            let target = self.kind(into + 1);
            let to_variable = matches!(target, Some(TokenKind::Variable | TokenKind::Parameter));
            if !to_variable {
                // PostgreSQL / T-SQL `SELECT ... INTO new_table`, MySQL `INTO OUTFILE`.
                i.is_write = true;
                if !self.any(into + 1, &["OUTFILE", "DUMPFILE"]) {
                    i.target = self.name_at(into + 1);
                }
            }
        }
        // Row locking clauses are rejected in read-only transactions.
        let locking = self.find_top(p + 1, |j| {
            (self.w(j, "FOR")
                && (self.any(j + 1, &["UPDATE", "SHARE"]) || (self.any(j + 1, &["NO", "KEY"]))))
                || (self.w(j, "LOCK") && self.w(j + 1, "IN"))
        });
        if locking.is_some() {
            i.is_write = true;
        }
        if i.target.is_none() {
            i.target = self.top_from_target(p + 1);
        }
        i
    }

    fn with(&self, p: usize, depth: u32) -> StatementInfo {
        let mut j = p + 1;
        if self.w(j, "RECURSIVE") {
            j += 1;
        }
        let mut cte_write = false;
        let mut cte_danger = None;
        // name [(cols)] AS [NOT] [MATERIALIZED] ( body ) [, ...]
        loop {
            if !matches!(
                self.kind(j),
                Some(TokenKind::Identifier | TokenKind::Keyword | TokenKind::QuotedIdentifier)
            ) {
                break;
            }
            j += 1;
            if self.kind(j) == Some(TokenKind::LParen) {
                j = self.close_paren(j) + 1;
            }
            if !self.w(j, "AS") {
                break;
            }
            j += 1;
            j = self.skip_words(j, &["NOT", "MATERIALIZED"]);
            if self.kind(j) != Some(TokenKind::LParen) {
                break;
            }
            let close = self.close_paren(j);
            let body = self.sub(j + 1, close).classify(0, depth + 1);
            cte_write |= body.is_write;
            if cte_danger.is_none() {
                cte_danger = body.danger;
            }
            j = close + 1;
            if self.kind(j) == Some(TokenKind::Comma) {
                j += 1;
                continue;
            }
            break;
        }
        let mut main = if j < self.t.len() {
            self.classify(j, depth + 1)
        } else {
            StatementInfo::new(StatementKind::Select, String::new()).rows()
        };
        if main.kind == StatementKind::Other && main.first_keyword.is_empty() {
            main.kind = StatementKind::Select;
        }
        main.first_keyword = "WITH".into();
        main.is_write |= cte_write;
        if main.danger.is_none() {
            main.danger = cte_danger;
        }
        main
    }

    fn set(&self, p: usize, first: String) -> StatementInfo {
        use StatementKind as K;
        let n = p + 1;
        let tx = self.w(n, "TRANSACTION")
            || (self.any(n, &["SESSION", "LOCAL", "GLOBAL"])
                && (self.w(n + 1, "TRANSACTION") || self.w(n + 1, "CHARACTERISTICS")));
        if tx {
            let mut i = StatementInfo::new(K::Transaction(TxCommand::SetTransaction), first);
            // READ WRITE lifts the read-only session of a read-only connection.
            i.is_write = self.w(n, "GLOBAL") || self.read_write(n);
            return i;
        }
        let global = self.any(n, &["GLOBAL", "PERSIST", "PERSIST_ONLY", "PASSWORD"])
            || self.text(n).to_ascii_lowercase().starts_with("@@global")
            || self.text(n).to_ascii_lowercase().starts_with("@@persist");
        // `SET default_transaction_read_only = off`, `SET SESSION transaction_read_only = 0`:
        // turning a read-only setting off (a value that is not plainly "on" counts as off).
        const READ_ONLY: [&str; 5] = [
            "default_transaction_read_only",
            "transaction_read_only",
            "tx_read_only",
            "read_only",
            "super_read_only",
        ];
        let read_only_setting = (n..self.t.len()).any(|k| {
            let name = self.text(k).trim_matches(['`', '"']).to_ascii_lowercase();
            let name = name
                .rsplit('.')
                .next()
                .unwrap_or("")
                .trim_start_matches('@');
            if !READ_ONLY.contains(&name) {
                return false;
            }
            let v = if self.text(k + 1) == "=" || self.w(k + 1, "TO") {
                k + 2
            } else {
                k + 1
            };
            let value = self.text(v).trim_matches(['\'', '"']).to_ascii_lowercase();
            !matches!(value.as_str(), "on" | "1" | "true" | "yes")
        });
        let mut i = StatementInfo::new(K::Set, first);
        i.is_write = global || read_only_setting;
        i
    }

    /// `READ WRITE` from token `from` on.
    fn read_write(&self, from: usize) -> bool {
        (from..self.t.len()).any(|k| self.w(k, "READ") && self.w(k + 1, "WRITE"))
    }

    fn explain(&self, p: usize, first: String, depth: u32) -> StatementInfo {
        let mut j = p + 1;
        let mut analyze = false;
        if self.kind(j) == Some(TokenKind::LParen) {
            let close = self.close_paren(j);
            for k in j + 1..close {
                if self.any(k, &["ANALYZE", "ANALYSE"])
                    && !self.any(k + 1, &["FALSE", "OFF", "0"])
                    && self.text(k + 1) != "0"
                {
                    analyze = true;
                }
            }
            j = close + 1;
        }
        const OPTS: &[&str] = &[
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
        for _ in 0..12 {
            if self.any(j, &["ANALYZE", "ANALYSE"]) {
                analyze = true;
                j += 1;
            } else if self.any(j, OPTS) || matches!(self.kind(j), Some(TokenKind::Operator)) {
                j += 1;
            } else {
                break;
            }
        }
        let mut i = StatementInfo::new(StatementKind::Explain, first).rows();
        if j >= self.t.len() {
            return i;
        }
        let inner = self.classify(j, depth + 1);
        if inner.kind == StatementKind::Other
            && !inner.first_keyword.is_empty()
            && self.kind(j + 1).is_none()
        {
            // MySQL `EXPLAIN tbl` describes a table.
            i.target = self.name_at(j);
            return i;
        }
        i.target = inner.target.clone();
        if analyze {
            i.is_write = inner.is_write;
            i.danger = inner.danger;
        }
        i
    }

    fn pragma(&self, p: usize, first: String) -> StatementInfo {
        let mut i = StatementInfo::new(StatementKind::Pragma, first);
        let assigns = self
            .find_top(p + 1, |j| {
                self.kind(j) == Some(TokenKind::Operator) && self.text(j) == "="
            })
            .is_some();
        // pragma name may be schema-qualified: main.table_info(x)
        let mut n = p + 1;
        if self.kind(n + 1) == Some(TokenKind::Dot) {
            n += 2;
        }
        let name = self.text(n).to_ascii_lowercase();
        let has_arg = self.kind(n + 1) == Some(TokenKind::LParen);
        if assigns || (has_arg && !QUERY_PRAGMAS.contains(&name.as_str())) {
            i.is_write = true;
        } else {
            i.returns_rows = true;
        }
        i
    }

    /// `CREATE/ALTER/DROP [modifiers] KIND [IF [NOT] EXISTS] [ONLY] name`.
    fn object_name_after_kind(&self, from: usize) -> Option<QualifiedName> {
        const KINDS: &[&str] = &[
            "TABLE",
            "VIEW",
            "INDEX",
            "SEQUENCE",
            "SCHEMA",
            "DATABASE",
            "FUNCTION",
            "PROCEDURE",
            "PROC",
            "TRIGGER",
            "TYPE",
            "DOMAIN",
            "EXTENSION",
            "ROLE",
            "USER",
            "EVENT",
            "RULE",
            "POLICY",
            "TABLESPACE",
            "SERVER",
            "PUBLICATION",
            "SUBSCRIPTION",
            "COLLATION",
            "AGGREGATE",
            "OPERATOR",
            "SYNONYM",
            "PACKAGE",
            "MACRO",
        ];
        let kind = (from..(from + 10).min(self.t.len())).find(|&j| self.any(j, KINDS))?;
        let mut j = self.skip_if_exists(kind + 1);
        j = self.skip_words(j, &["CONCURRENTLY", "ONLY"]);
        j = self.skip_if_exists(j);
        if self.w(j, "ON") {
            return None;
        }
        self.name_at(j)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use StatementKind as K;

    fn c(sql: &str) -> StatementInfo {
        classify(sql, &pg())
    }
    fn cm(sql: &str) -> StatementInfo {
        classify(sql, &mysql())
    }

    #[test]
    fn later_tsql_statements_and_read_write() {
        let t = |sql: &str| classify(sql, &tsql());
        for sql in [
            "SELECT 1\nDELETE FROM t",
            "SET NOCOUNT ON\nUPDATE t SET a = 1",
            "DECLARE @x INT\nDROP TABLE t",
            "PRINT 'x'\nTRUNCATE TABLE t",
        ] {
            assert!(t(sql).is_write, "{sql}");
        }
        assert!(!t("SELECT a FROM t WHERE b IN (SELECT b FROM u)").is_write);
        assert!(!t("SELECT * FROM a INNER MERGE JOIN b ON a.id = b.id").is_write);
        // Each statement of a run is checked on its own.
        assert_eq!(
            t("UPDATE orders SET status = 'x'\nDELETE FROM log WHERE id = 5").danger,
            Some(Danger::UpdateWithoutWhere)
        );
        assert_eq!(
            t("UPDATE t SET a = 1 WHERE id = 2\nDELETE FROM log").danger,
            Some(Danger::DeleteWithoutWhere)
        );
        assert_eq!(
            t("SELECT 1\nUPDATE t SET a = 1 WHERE id = 2\nDELETE FROM log").danger,
            Some(Danger::DeleteWithoutWhere)
        );
        assert_eq!(
            t("UPDATE t SET a = 1 WHERE id = 2\nSELECT * FROM t").danger,
            None
        );
        assert_eq!(
            t("UPDATE t WITH (ROWLOCK) SET a = 1 WHERE id = 2").danger,
            None
        );
        assert_eq!(t("INSERT INTO t EXEC dbo.p").danger, None);
        assert_eq!(t("INSERT INTO t SELECT a FROM u WHERE b = 1").danger, None);
        assert_eq!(t("MERGE t USING s ON t.id = s.id WHEN MATCHED THEN DELETE WHEN NOT MATCHED THEN INSERT (id) VALUES (s.id);").danger, None);
        assert!(c("SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE").is_write);
        assert!(c("SET default_transaction_read_only = off").is_write);
        assert!(cm("SET SESSION transaction_read_only = 0").is_write);
        assert!(!c("SET default_transaction_read_only = on").is_write);
        assert!(!c("SET my_app.read_only_mode = 'x'").is_write);
        assert!(c("BEGIN READ WRITE").is_write);
        assert!(c("START TRANSACTION READ WRITE").is_write);
        assert!(cm("SET SESSION TRANSACTION READ WRITE").is_write);
        assert!(
            !c("BEGIN").is_write && !c("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE").is_write
        );
        assert!(cm("SELECT * FROM users /*!INTO OUTFILE '/tmp/x.csv'*/").is_write);
        assert!(!cm("SELECT 1 /*!50000 , 2 */").is_write);
    }

    #[test]
    fn queries() {
        let i = c("SELECT * FROM public.users WHERE id = 1");
        assert_eq!(i.kind, K::Select);
        assert!(i.returns_rows && !i.is_write && i.danger.is_none());
        assert_eq!(i.first_keyword, "SELECT");
        assert_eq!(
            i.target,
            Some(QualifiedName {
                catalog: None,
                schema: Some("public".into()),
                name: "users".into()
            })
        );
        let i = c("-- comment\n/* x */ ((SELECT 1)) UNION (SELECT 2)");
        assert_eq!(
            (i.kind, i.returns_rows, i.is_write),
            (K::Select, true, false)
        );
        assert_eq!(c("VALUES (1), (2)").kind, K::Select);
        let i = c("TABLE users");
        assert_eq!((i.kind, i.returns_rows), (K::Select, true));
        assert_eq!(i.target.map(|t| t.name).as_deref(), Some("users"));
        let i = c("WITH x AS (SELECT 1) SELECT * FROM x");
        assert_eq!(
            (i.kind, i.returns_rows, i.is_write, i.first_keyword.as_str()),
            (K::Select, true, false, "WITH")
        );
        let i =
            c("WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t) SELECT * FROM t");
        assert!(!i.is_write && i.returns_rows);
        for q in [
            "SHOW search_path",
            "DESCRIBE users",
            "DESC users",
            "EXPLAIN SELECT 1",
            "EXPLAIN DELETE FROM t",
        ] {
            let i = cm(q);
            assert!(i.returns_rows && !i.is_write, "{q}: {i:?}");
        }
    }

    #[test]
    fn select_into_and_locking() {
        let i = c("SELECT * INTO new_table FROM t");
        assert!(i.is_write && !i.returns_rows);
        assert_eq!(i.target.map(|t| t.name).as_deref(), Some("new_table"));
        let i = cm("SELECT a INTO @v FROM t LIMIT 1");
        assert!(!i.is_write && !i.returns_rows);
        assert!(cm("SELECT * FROM t INTO OUTFILE '/tmp/x'").is_write);
        assert!(c("SELECT * FROM t FOR UPDATE").is_write);
        assert!(c("SELECT * FROM t FOR NO KEY UPDATE SKIP LOCKED").is_write);
        // INTO inside a subquery does not count
        assert!(!c("SELECT (SELECT 1) FROM t").is_write);
    }

    #[test]
    fn writes_and_cte_writes() {
        let i = c("INSERT INTO s.t (a) VALUES (1) RETURNING id");
        assert_eq!(
            (i.kind, i.is_write, i.returns_rows),
            (K::Insert, true, true)
        );
        assert_eq!(
            i.target,
            Some(QualifiedName {
                catalog: None,
                schema: Some("s".into()),
                name: "t".into()
            })
        );
        let i = cm("REPLACE INTO t VALUES (1)");
        assert_eq!((i.kind, i.is_write), (K::Insert, true));
        let i = c("WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d");
        assert!(i.is_write && i.returns_rows);
        assert_eq!(i.danger, Some(Danger::DeleteWithoutWhere));
        let i = c("WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x");
        assert_eq!(
            (i.kind, i.is_write, i.first_keyword.as_str()),
            (K::Insert, true, "WITH")
        );
        let i = c("WITH x AS (SELECT id FROM a) UPDATE t SET v = 1 FROM x WHERE t.id = x.id");
        assert_eq!((i.kind, i.is_write, i.danger), (K::Update, true, None));
        let i = c("MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET v = s.v");
        assert_eq!((i.kind, i.is_write), (K::Merge, true));
        assert_eq!(i.target.map(|t| t.name).as_deref(), Some("t"));
    }

    #[test]
    fn dangers() {
        assert_eq!(c("DELETE FROM t").danger, Some(Danger::DeleteWithoutWhere));
        assert_eq!(c("DELETE FROM t WHERE id = 1").danger, None);
        assert_eq!(
            c("delete from t where id in (select id from u)").danger,
            None
        );
        assert_eq!(
            c("UPDATE t SET a = 1").danger,
            Some(Danger::UpdateWithoutWhere)
        );
        assert_eq!(
            c("UPDATE t SET a = (SELECT b FROM u WHERE u.id = t.id)").danger,
            Some(Danger::UpdateWithoutWhere)
        );
        assert_eq!(c("UPDATE t SET a = 1 WHERE id = 2").danger, None);
        assert_eq!(
            cm("DELETE FROM t LIMIT 10").danger,
            Some(Danger::DeleteWithoutWhere)
        );
        assert_eq!(c("DROP TABLE IF EXISTS a, b").danger, Some(Danger::Drop));
        assert_eq!(
            c("DROP TABLE IF EXISTS a, b")
                .target
                .map(|t| t.name)
                .as_deref(),
            Some("a")
        );
        assert_eq!(c("TRUNCATE TABLE t").danger, Some(Danger::Truncate));
        assert_eq!(c("TRUNCATE t").target.map(|t| t.name).as_deref(), Some("t"));
        assert_eq!(
            c("ALTER TABLE t DROP COLUMN x").danger,
            Some(Danger::AlterDrop)
        );
        assert_eq!(c("ALTER TABLE t ALTER COLUMN x DROP NOT NULL").danger, None);
        assert_eq!(c("ALTER TABLE t ALTER COLUMN x DROP DEFAULT").danger, None);
        assert_eq!(c("ALTER TABLE t ADD COLUMN y int").danger, None);
        assert_eq!(
            c("EXPLAIN ANALYZE DELETE FROM t").danger,
            Some(Danger::DeleteWithoutWhere)
        );
        assert_eq!(c("EXPLAIN DELETE FROM t").danger, None);
    }

    #[test]
    fn explain_analyze() {
        let i = c("EXPLAIN ANALYZE DELETE FROM t WHERE id = 1");
        assert_eq!(
            (i.kind, i.is_write, i.returns_rows),
            (K::Explain, true, true)
        );
        assert!(c("EXPLAIN (ANALYZE, BUFFERS) UPDATE t SET a = 1 WHERE b = 2").is_write);
        assert!(!c("EXPLAIN (ANALYZE false) UPDATE t SET a = 1 WHERE b = 2").is_write);
        assert!(!c("EXPLAIN ANALYZE SELECT 1").is_write);
        assert!(!c("EXPLAIN UPDATE t SET a = 1").is_write);
        assert!(!cm("EXPLAIN FORMAT=JSON SELECT 1").is_write);
        let i = cm("EXPLAIN users");
        assert_eq!(i.target.map(|t| t.name).as_deref(), Some("users"));
        assert!(!classify("EXPLAIN QUERY PLAN SELECT 1", &sqlite()).is_write);
    }

    #[test]
    fn transactions_and_session() {
        for (q, tx) in [
            ("BEGIN", TxCommand::Begin),
            (
                "BEGIN TRANSACTION ISOLATION LEVEL SERIALIZABLE",
                TxCommand::Begin,
            ),
            ("START TRANSACTION READ ONLY", TxCommand::Begin),
            ("COMMIT", TxCommand::Commit),
            ("END", TxCommand::Commit),
            ("ROLLBACK TO SAVEPOINT a", TxCommand::Rollback),
            ("SAVEPOINT a", TxCommand::Savepoint),
            ("RELEASE SAVEPOINT a", TxCommand::Release),
            (
                "SET TRANSACTION ISOLATION LEVEL READ COMMITTED",
                TxCommand::SetTransaction,
            ),
            (
                "SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY",
                TxCommand::SetTransaction,
            ),
        ] {
            let i = c(q);
            assert_eq!(i.kind, K::Transaction(tx), "{q}");
            assert!(!i.is_write && i.is_transaction_control(), "{q}");
        }
        // A block that merely starts with BEGIN is not a transaction
        let i = classify("BEGIN INSERT INTO t VALUES (1); END", &tsql());
        assert!(i.is_write && i.kind == K::Other);
        let i = c("SET search_path TO app");
        assert_eq!((i.kind, i.is_write), (K::Set, false));
        assert!(cm("SET GLOBAL max_connections = 10").is_write);
        assert!(cm("SET @@global.sql_mode = ''").is_write);
        assert!(!cm("SET @x = 1").is_write);
        assert!(cm("SET PASSWORD = 'x'").is_write);
        let i = cm("USE shop");
        assert_eq!((i.kind, i.is_write), (K::Use, false));
        assert!(!c("RESET ALL").is_write);
        assert!(cm("RESET MASTER").is_write);
    }

    #[test]
    fn calls_copy_maintenance_pragma_lock() {
        let i = c("CALL do_things(1)");
        assert_eq!((i.kind, i.is_write), (K::Call, true));
        assert_eq!(i.target.map(|t| t.name).as_deref(), Some("do_things"));
        assert!(classify("EXEC dbo.p @a = 1", &tsql()).is_write);
        assert!(c("DO $$ BEGIN END $$").is_write);
        assert!(c("COPY t FROM '/tmp/x.csv'").is_write);
        assert!(c("COPY t FROM STDIN").is_write);
        assert!(!c("COPY (SELECT 1) TO STDOUT").is_write);
        assert!(c("COPY t TO '/tmp/out.csv'").is_write);
        for q in [
            "VACUUM ANALYZE t",
            "ANALYZE t",
            "REINDEX TABLE t",
            "CLUSTER t",
            "REFRESH MATERIALIZED VIEW v",
        ] {
            let i = c(q);
            assert_eq!((i.kind, i.is_write), (K::Maintenance, true), "{q}");
        }
        for q in ["OPTIMIZE TABLE t", "CHECK TABLE t", "REPAIR TABLE t"] {
            assert_eq!(cm(q).kind, K::Maintenance, "{q}");
        }
        let s = sqlite();
        let i = classify("PRAGMA foreign_keys = ON", &s);
        assert_eq!(
            (i.kind, i.is_write, i.returns_rows),
            (K::Pragma, true, false)
        );
        let i = classify("PRAGMA table_info(users)", &s);
        assert_eq!((i.is_write, i.returns_rows), (false, true));
        let i = classify("PRAGMA journal_mode", &s);
        assert_eq!((i.is_write, i.returns_rows), (false, true));
        assert!(classify("PRAGMA cache_size(1000)", &s).is_write);
        let i = c("LOCK TABLE t IN ACCESS EXCLUSIVE MODE");
        assert_eq!((i.kind, i.is_write), (K::Lock, true));
        assert!(!cm("UNLOCK TABLES").is_write);
    }

    #[test]
    fn ddl_targets() {
        let t = |q: &str| c(q).target.map(|t| (t.schema, t.name));
        assert_eq!(
            t("CREATE TABLE IF NOT EXISTS app.items (id int)"),
            Some((Some("app".into()), "items".into()))
        );
        assert_eq!(
            t("CREATE OR REPLACE VIEW v AS SELECT 1"),
            Some((None, "v".into()))
        );
        assert_eq!(
            t("CREATE UNIQUE INDEX CONCURRENTLY idx ON t (a)"),
            Some((None, "idx".into()))
        );
        assert_eq!(t("CREATE INDEX ON t (a)"), None);
        assert_eq!(
            t("CREATE TEMP TABLE \"Odd Name\" (a int)"),
            Some((None, "Odd Name".into()))
        );
        assert_eq!(
            t("DROP VIEW IF EXISTS s.v CASCADE"),
            Some((Some("s".into()), "v".into()))
        );
        assert_eq!(
            t("ALTER TABLE ONLY s.t ADD COLUMN x int"),
            Some((Some("s".into()), "t".into()))
        );
        assert_eq!(c("CREATE TABLE t (a int)").kind, K::Create);
        assert_eq!(c("COMMENT ON TABLE t IS 'x'").kind, K::Comment);
        assert_eq!(c("GRANT SELECT ON t TO bob").kind, K::Grant);
        assert_eq!(c("REVOKE ALL ON t FROM bob").kind, K::Revoke);
        assert_eq!(cm("RENAME TABLE a TO b").kind, K::Rename);
        let i = classify("SELECT * FROM db.dbo.t", &tsql());
        assert_eq!(
            i.target,
            Some(QualifiedName {
                catalog: Some("db".into()),
                schema: Some("dbo".into()),
                name: "t".into()
            })
        );
    }

    #[test]
    fn special_statements() {
        let i = c("@set x = 1");
        assert_eq!(
            (i.kind, i.is_write, i.first_keyword.as_str()),
            (K::ClientCommand, false, "@SET")
        );
        let i = c("");
        assert_eq!((i.kind, i.is_write), (K::Other, false));
        let i = c("-- only a comment");
        assert!(!i.is_write);
        let i = cm("/*!40101 SET NAMES utf8 */");
        assert_eq!((i.kind, i.is_write), (K::Set, false));
        let i = cm("/*!40000 ALTER TABLE `t` DISABLE KEYS */");
        assert_eq!((i.kind, i.is_write), (K::Alter, true));
        // Unknown statements are writes
        assert!(c("FROBNICATE everything").is_write);
        assert!(c("1 + 1").is_write);
        let i = c("PREPARE q AS SELECT 1");
        assert!(!i.is_write);
        assert!(c("PREPARE q AS DELETE FROM t").is_write);
        assert!(!c("DECLARE c CURSOR FOR SELECT * FROM t").is_write);
        assert!(!classify("DECLARE @x INT", &tsql()).is_write);
        let i = c("FETCH 10 FROM c");
        assert!(i.returns_rows && !i.is_write);
    }

    #[test]
    fn split_and_classify_script() {
        let src = "SELECT 1;\nDELETE FROM t;\n@set a = 1\nUPDATE t SET x = 1 WHERE y = 2";
        let r = split_and_classify(src, &pg(), &SplitOptions::default());
        let kinds: Vec<_> = r.iter().map(|(_, i)| i.kind).collect();
        assert_eq!(
            kinds,
            vec![K::Select, K::Delete, K::ClientCommand, K::Update]
        );
        assert_eq!(r[1].1.danger, Some(Danger::DeleteWithoutWhere));
        assert_eq!(r[3].1.danger, None);
    }

    #[test]
    fn random_inputs_never_panic() {
        let mut rng = Rng::new(11);
        for _ in 0..500 {
            let src = random_sql(&mut rng, 30);
            for syn in all_syntaxes() {
                let _ = classify(&src, &syn);
            }
        }
    }
}
