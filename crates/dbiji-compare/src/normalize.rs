//! Text normalisation used by schema compare.
//!
//! * a small, dialect tolerant SQL tokenizer (strings, quoted identifiers,
//!   comments, dollar quoting),
//! * canonical forms of SQL definitions (views, routines, triggers, checks,
//!   defaults) so that whitespace, keyword case, redundant quoting and
//!   qualification with the object's own schema do not show up as differences,
//! * canonical data type names (`int4` = `integer`, `varchar` = `character varying` ...),
//! * schema renaming inside definitions (`staging.users` -> `prod.users`),
//! * extraction of referenced object names (view dependencies, sequences used
//!   by defaults, routines called by expressions).

use std::collections::BTreeSet;

/// Kind of a lexical token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokKind {
    Space,
    Comment,
    /// Unquoted word: keyword or identifier.
    Word,
    /// Quoted identifier; the payload is the closing quote character.
    Quoted(char),
    /// String literal (including `E'..'`, `N'..'`, `$tag$..$tag$`).
    Str,
    Number,
    Punct,
}

/// A token as a byte range of the source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Token {
    pub kind: TokKind,
    pub start: usize,
    pub end: usize,
}

const MULTI_PUNCT: &[&str] = &[
    "->>", "#>>", "::", "<=", ">=", "<>", "!=", "||", "->", "#>", "=>", ":=", "**", "@>", "<@",
];

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

fn is_ident_char(c: char) -> bool {
    c == '_' || c == '$' || c.is_alphanumeric()
}

/// Split SQL text into tokens. Never fails: unterminated constructs extend to
/// the end of the input.
pub(crate) fn tokenize(text: &str) -> Vec<Token> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    let len = text.len();
    let push = |out: &mut Vec<Token>, kind, start, end| out.push(Token { kind, start, end });
    while i < len {
        let Some(c) = text[i..].chars().next() else { break };
        let start = i;
        if c.is_whitespace() {
            i += c.len_utf8();
            while i < len {
                match text[i..].chars().next() {
                    Some(n) if n.is_whitespace() => i += n.len_utf8(),
                    _ => break,
                }
            }
            push(&mut out, TokKind::Space, start, i);
            continue;
        }
        // -- line comment
        if c == '-' && bytes.get(i + 1) == Some(&b'-') {
            i = text[i..].find('\n').map(|p| i + p).unwrap_or(len);
            push(&mut out, TokKind::Comment, start, i);
            continue;
        }
        // /* block comment */ (nesting tolerated)
        if c == '/' && bytes.get(i + 1) == Some(&b'*') {
            let mut depth = 0usize;
            while i < len {
                if text[i..].starts_with("/*") {
                    depth += 1;
                    i += 2;
                } else if text[i..].starts_with("*/") {
                    depth = depth.saturating_sub(1);
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += text[i..].chars().next().map_or(1, char::len_utf8);
                }
            }
            push(&mut out, TokKind::Comment, start, i.min(len));
            continue;
        }
        // string literal, optionally prefixed (E'', N'', X'', B'', U&'')
        if c == '\'' {
            i = scan_string(text, i + 1, false);
            push(&mut out, TokKind::Str, start, i);
            continue;
        }
        if (c == 'E' || c == 'e') && bytes.get(i + 1) == Some(&b'\'') {
            i = scan_string(text, i + 2, true);
            push(&mut out, TokKind::Str, start, i);
            continue;
        }
        if matches!(c, 'N' | 'n' | 'X' | 'x' | 'B' | 'b') && bytes.get(i + 1) == Some(&b'\'') {
            i = scan_string(text, i + 2, false);
            push(&mut out, TokKind::Str, start, i);
            continue;
        }
        // dollar quoting $tag$ ... $tag$  (but not $1 parameters)
        if c == '$' {
            if let Some(end_tag) = dollar_tag(text, i) {
                let body_start = i + end_tag.len();
                i = text[body_start..].find(end_tag).map(|p| body_start + p + end_tag.len()).unwrap_or(len);
                push(&mut out, TokKind::Str, start, i);
                continue;
            }
            // positional parameter or stray dollar
            i += 1;
            while i < len && bytes[i].is_ascii_digit() {
                i += 1;
            }
            push(&mut out, TokKind::Word, start, i);
            continue;
        }
        // quoted identifiers
        if c == '"' || c == '`' {
            i = scan_quoted(text, i + 1, c);
            push(&mut out, TokKind::Quoted(c), start, i);
            continue;
        }
        if c == '['
            && let Some(end) = bracket_identifier_end(text, i) {
                i = end;
                push(&mut out, TokKind::Quoted(']'), start, i);
                continue;
            }
        if c.is_ascii_digit() || (c == '.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            i = scan_number(text, i);
            push(&mut out, TokKind::Number, start, i);
            continue;
        }
        if is_ident_start(c) {
            i += c.len_utf8();
            while i < len {
                match text[i..].chars().next() {
                    Some(n) if is_ident_char(n) => i += n.len_utf8(),
                    _ => break,
                }
            }
            push(&mut out, TokKind::Word, start, i);
            continue;
        }
        if let Some(p) = MULTI_PUNCT.iter().find(|p| text[i..].starts_with(**p)) {
            i += p.len();
        } else {
            i += c.len_utf8();
        }
        push(&mut out, TokKind::Punct, start, i);
    }
    out
}

fn scan_string(text: &str, mut i: usize, backslash: bool) -> usize {
    let bytes = text.as_bytes();
    let len = text.len();
    while i < len {
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
    len
}

fn scan_quoted(text: &str, mut i: usize, quote: char) -> usize {
    let q = quote as u8;
    let bytes = text.as_bytes();
    let len = text.len();
    while i < len {
        if bytes[i] == q {
            if bytes.get(i + 1) == Some(&q) {
                i += 2;
            } else {
                return i + 1;
            }
        } else {
            i += 1;
        }
    }
    len
}

/// `[name]` is treated as a quoted identifier (SQL Server) only when its
/// content looks like an identifier; `arr[1]` and `ARRAY[1, 2]` stay punctuation.
fn bracket_identifier_end(text: &str, i: usize) -> Option<usize> {
    let rest = &text[i + 1..];
    let close = rest.find(']')?;
    let content = &rest[..close];
    let mut chars = content.chars();
    let first = chars.next()?;
    if !(first.is_alphabetic() || first == '_' || first == '#' || first == '@') {
        return None;
    }
    if content.chars().all(|c| c.is_alphanumeric() || " _$#@-.".contains(c)) {
        Some(i + 1 + close + 1)
    } else {
        None
    }
}

fn scan_number(text: &str, mut i: usize) -> usize {
    let bytes = text.as_bytes();
    let len = text.len();
    while i < len && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
        i += 1;
    }
    if i < len && (bytes[i] == b'e' || bytes[i] == b'E') {
        let mut j = i + 1;
        if j < len && (bytes[j] == b'+' || bytes[j] == b'-') {
            j += 1;
        }
        if j < len && bytes[j].is_ascii_digit() {
            i = j;
            while i < len && bytes[i].is_ascii_digit() {
                i += 1;
            }
        }
    }
    i
}

/// When a dollar quote starts at `i`, returns the full tag (`$$` or `$tag$`).
fn dollar_tag(text: &str, i: usize) -> Option<&str> {
    let rest = &text[i + 1..];
    let mut end = None;
    for (pos, c) in rest.char_indices() {
        if c == '$' {
            end = Some(pos);
            break;
        }
        if !(c == '_' || c.is_alphanumeric()) || (pos == 0 && c.is_ascii_digit()) {
            return None;
        }
    }
    let end = end?;
    Some(&text[i..i + 1 + end + 1])
}

/// Tag and body of a dollar-quoted string token (`$fn$ body $fn$`).
pub(crate) fn dollar_body(token_text: &str) -> Option<(&str, &str)> {
    if !token_text.starts_with('$') {
        return None;
    }
    let tag = dollar_tag(token_text, 0)?;
    if token_text.len() < 2 * tag.len() || !token_text.ends_with(tag) {
        return None;
    }
    Some((tag, &token_text[tag.len()..token_text.len() - tag.len()]))
}

/// Content of a quoted identifier token without quotes, doubled quotes collapsed.
pub(crate) fn unquote(token_text: &str, close: char) -> String {
    let inner = if token_text.len() >= 2 && token_text.ends_with(close) {
        &token_text[1..token_text.len() - close.len_utf8()]
    } else {
        token_text.get(1..).unwrap_or("")
    };
    let doubled: String = [close, close].iter().collect();
    inner.replace(&doubled, &close.to_string())
}

/// Content of a simple `'...'` string literal (doubled quotes collapsed);
/// `None` for other literal forms.
pub(crate) fn string_content(token_text: &str) -> Option<String> {
    let inner = token_text.strip_prefix('\'')?;
    let inner = inner.strip_suffix('\'').unwrap_or(inner);
    Some(inner.replace("''", "'"))
}

fn is_simple_lower_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_lowercase() => {}
        _ => return false,
    }
    s.chars().all(|c| c == '_' || c == '$' || c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// A token after normalisation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NTok {
    /// Case-insensitive word (already lower case).
    Word(String),
    /// Case-sensitive identifier (from a quoted identifier).
    Ident(String),
    /// Literal kept verbatim (strings, numbers).
    Lit(String),
    Punct(String),
}

impl NTok {
    fn name(&self) -> Option<String> {
        match self {
            NTok::Word(w) => Some(w.clone()),
            NTok::Ident(i) => Some(i.to_lowercase()),
            _ => None,
        }
    }
    fn is_punct(&self, p: &str) -> bool {
        matches!(self, NTok::Punct(x) if x == p)
    }
    fn is_word(&self, w: &str) -> bool {
        matches!(self, NTok::Word(x) if x == w)
    }
}

/// Settings for [`normalize_sql`].
#[derive(Debug, Clone, Default)]
pub(crate) struct SqlNorm {
    /// Schema of the object the text belongs to; qualifiers equal to it are removed.
    pub own_schema: Option<String>,
    /// Remove `::type` casts (cross-dialect comparisons).
    pub strip_casts: bool,
    /// Map well known function spellings to one form (cross-dialect comparisons).
    pub canonical_functions: bool,
}

fn ntokens(text: &str) -> Vec<NTok> {
    tokenize(text)
        .into_iter()
        .filter_map(|t| {
            let s = &text[t.start..t.end];
            match t.kind {
                TokKind::Space | TokKind::Comment => None,
                TokKind::Word => Some(NTok::Word(s.to_lowercase())),
                TokKind::Quoted(close) => {
                    let content = unquote(s, close);
                    if is_simple_lower_ident(&content) {
                        Some(NTok::Word(content))
                    } else {
                        Some(NTok::Ident(content))
                    }
                }
                TokKind::Str | TokKind::Number => Some(NTok::Lit(s.to_string())),
                TokKind::Punct => Some(NTok::Punct(s.to_string())),
            }
        })
        .collect()
}

/// Remove `DEFINER = user@host` (MySQL) which differs between servers.
fn strip_definer(toks: &mut Vec<NTok>) {
    let mut i = 0;
    while i < toks.len() {
        if toks[i].is_word("definer") && toks.get(i + 1).is_some_and(|t| t.is_punct("=")) {
            let mut j = i + 2;
            if j < toks.len() && !toks[j].is_punct("(") {
                j += 1;
            }
            if toks.get(j).is_some_and(|t| t.is_punct("@")) {
                j += 2;
            } else if toks.get(j).is_some_and(|t| t.is_punct("(")) && toks.get(j + 1).is_some_and(|t| t.is_punct(")")) {
                // CURRENT_USER()
                j += 2;
            }
            toks.drain(i..j.min(toks.len()));
            continue;
        }
        i += 1;
    }
}

fn strip_qualifier(toks: &mut Vec<NTok>, schema: &str) {
    let schema_lc = schema.to_lowercase();
    let mut i = 0;
    while i < toks.len() {
        if i + 1 < toks.len()
            && toks[i].name().as_deref() == Some(schema_lc.as_str())
            && toks[i + 1].is_punct(".")
            && toks.get(i + 2).is_some_and(|t| t.name().is_some() || t.is_punct("*"))
            && (i == 0 || !toks[i - 1].is_punct("."))
        {
            toks.drain(i..i + 2);
            continue;
        }
        i += 1;
    }
    // qualified names inside string literals: nextval('schema.seq'), 'schema.t'::regclass
    let plain = format!("'{schema_lc}.");
    let quoted = format!("'\"{schema}\".");
    for t in toks.iter_mut() {
        if let NTok::Lit(s) = t {
            let rest = if s.to_lowercase().starts_with(&plain) {
                s.get(plain.len()..)
            } else if s.starts_with(&quoted) {
                s.get(quoted.len()..)
            } else {
                None
            };
            if let Some(rest) = rest {
                *s = format!("'{rest}");
            }
        }
    }
}

fn strip_casts(toks: &mut Vec<NTok>) {
    let mut i = 0;
    while i < toks.len() {
        if toks[i].is_punct("::") {
            let mut j = i + 1;
            // type words, e.g. `character varying`, `timestamp with time zone`
            while j < toks.len() && matches!(toks[j], NTok::Word(_) | NTok::Ident(_)) {
                j += 1;
                if !matches!(toks.get(j), Some(NTok::Word(w)) if ["varying", "precision", "with", "without", "time", "zone"].contains(&w.as_str()))
                {
                    break;
                }
            }
            if toks.get(j).is_some_and(|t| t.is_punct("("))
                && let Some(close) = matching_paren(toks, j) {
                    j = close + 1;
                }
            while toks.get(j).is_some_and(|t| t.is_punct("[")) && toks.get(j + 1).is_some_and(|t| t.is_punct("]")) {
                j += 2;
            }
            toks.drain(i..j);
            continue;
        }
        i += 1;
    }
}

fn matching_paren(toks: &[NTok], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (k, t) in toks.iter().enumerate().skip(open) {
        if t.is_punct("(") {
            depth += 1;
        } else if t.is_punct(")") {
            depth -= 1;
            if depth == 0 {
                return Some(k);
            }
        }
    }
    None
}

fn strip_outer_parens(toks: &mut Vec<NTok>) {
    while toks.len() >= 2 && toks[0].is_punct("(") && matching_paren(toks, 0) == Some(toks.len() - 1) {
        toks.pop();
        toks.remove(0);
    }
}

fn render(toks: &[NTok]) -> String {
    let mut out = String::new();
    for (i, t) in toks.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        match t {
            NTok::Word(w) => out.push_str(w),
            NTok::Ident(s) => {
                out.push('"');
                out.push_str(&s.replace('"', "\"\""));
                out.push('"');
            }
            NTok::Lit(s) | NTok::Punct(s) => out.push_str(s),
        }
    }
    out
}

/// Replace well known function spellings by one canonical word (token based,
/// so `snow()` is never mistaken for `now()`).
fn canonical_functions(toks: &mut Vec<NTok>) {
    const SAME: &[(&[&str], &str)] = &[
        (&["now", "(", ")"], "current_timestamp"),
        (&["current_timestamp", "(", ")"], "current_timestamp"),
        (&["getdate", "(", ")"], "current_timestamp"),
        (&["sysdatetime", "(", ")"], "current_timestamp"),
        (&["localtimestamp"], "current_timestamp"),
        (&["datetime", "(", "'now'", ")"], "current_timestamp"),
        (&["curdate", "(", ")"], "current_date"),
        (&["current_date", "(", ")"], "current_date"),
        (&["gen_random_uuid", "(", ")"], "uuid()"),
        (&["uuid", "(", ")"], "uuid()"),
        (&["newid", "(", ")"], "uuid()"),
    ];
    let text_of = |t: &NTok| -> String {
        match t {
            NTok::Word(w) => w.clone(),
            NTok::Ident(i) => i.to_lowercase(),
            NTok::Lit(l) => l.to_lowercase(),
            NTok::Punct(p) => p.clone(),
        }
    };
    let mut i = 0;
    while i < toks.len() {
        if let Some((pattern, canon)) = SAME
            .iter()
            .find(|(pattern, _)| i + pattern.len() <= toks.len() && pattern.iter().enumerate().all(|(k, p)| text_of(&toks[i + k]) == *p))
        {
            toks.splice(i..i + pattern.len(), [NTok::Word((*canon).to_string())]);
        }
        i += 1;
    }
}

/// Canonical form of a SQL fragment for comparison purposes (not for display).
pub(crate) fn normalize_sql(text: &str, n: &SqlNorm) -> String {
    let mut toks = ntokens(text);
    // routine bodies in dollar quotes are SQL too
    for t in toks.iter_mut() {
        if let NTok::Lit(lit) = t
            && let Some((_, body)) = dollar_body(lit) {
                *t = NTok::Lit(format!("$$ {} $$", normalize_sql(body, n)));
            }
    }
    strip_definer(&mut toks);
    if let Some(schema) = n.own_schema.as_deref().filter(|s| !s.is_empty()) {
        strip_qualifier(&mut toks, schema);
    }
    if n.strip_casts {
        strip_casts(&mut toks);
    }
    while toks.last().is_some_and(|t| t.is_punct(";")) {
        toks.pop();
    }
    if n.canonical_functions {
        canonical_functions(&mut toks);
    }
    strip_outer_parens(&mut toks);
    render(&toks)
}

/// Light normalisation when SQL normalisation is disabled: line endings,
/// surrounding whitespace and trailing semicolons only.
pub(crate) fn trim_sql(text: &str) -> String {
    let mut s = text.replace("\r\n", "\n");
    loop {
        let t = s.trim_end();
        if let Some(rest) = t.strip_suffix(';') {
            s = rest.to_string();
        } else {
            s = t.to_string();
            break;
        }
    }
    s.trim().to_string()
}

/// Canonical spelling of a column data type.
pub(crate) fn normalize_type(data_type: &str, aliases: bool) -> String {
    let lower = data_type.trim().to_lowercase();
    let collapsed = lower.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut s = collapsed.replace(" (", "(").replace("( ", "(").replace(" )", ")").replace(", ", ",").replace(" ,", ",");
    if !aliases {
        return s;
    }
    // array suffixes
    let mut dims = 0;
    loop {
        let t = s.trim_end();
        if let Some(rest) = t.strip_suffix("[]") {
            dims += 1;
            s = rest.to_string();
        } else {
            s = t.to_string();
            break;
        }
    }
    let (head, mods, tail) = match s.find('(') {
        Some(open) => match s[open..].find(')') {
            Some(close) => {
                let close = open + close;
                (s[..open].trim().to_string(), s[open..=close].to_string(), s[close + 1..].trim().to_string())
            }
            None => (s.clone(), String::new(), String::new()),
        },
        None => (s.clone(), String::new(), String::new()),
    };
    // split "timestamp with time zone" style suffixes out of the head
    let mut head = head;
    let mut tail = tail;
    // MySQL attribute words: `int unsigned zerofill`
    loop {
        let mut moved = false;
        for word in [" unsigned", " zerofill", " signed"] {
            if let Some(base) = head.strip_suffix(word) {
                tail = if tail.is_empty() { word.trim().to_string() } else { format!("{} {tail}", word.trim()) };
                head = base.to_string();
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
    for suffix in [" with time zone", " without time zone"] {
        if let Some(base) = head.strip_suffix(suffix) {
            tail = format!("{}{}", suffix.trim(), if tail.is_empty() { String::new() } else { format!(" {tail}") });
            head = base.to_string();
        }
    }
    let with_tz = tail.starts_with("with time zone");
    let without_tz = tail.starts_with("without time zone");
    let rest_tail = tail
        .strip_prefix("with time zone")
        .or_else(|| tail.strip_prefix("without time zone"))
        .unwrap_or(&tail)
        .trim()
        .to_string();
    let base = match head.as_str() {
        "int" | "integer" | "int4" | "signed" => "integer",
        "int8" | "bigint" => "bigint",
        "int2" | "smallint" => "smallint",
        "float8" | "double precision" | "double" => "double precision",
        "float4" | "real" => "real",
        "bool" | "boolean" => "boolean",
        "varchar" | "character varying" | "char varying" => "varchar",
        "char" | "character" | "bpchar" => "char",
        "decimal" | "numeric" | "dec" => "numeric",
        "varbit" | "bit varying" => "varbit",
        "timestamptz" => "timestamptz",
        "timetz" => "timetz",
        "timestamp" if with_tz => "timestamptz",
        "time" if with_tz => "timetz",
        other => other,
    }
    .to_string();
    let keep_tail = if (head == "timestamp" || head == "time") && (with_tz || without_tz) { rest_tail } else { tail.clone() };
    // MySQL integer display widths: int(11) == int, except tinyint(1) (boolean)
    let int_like = ["integer", "bigint", "smallint", "mediumint", "tinyint"].contains(&base.as_str());
    let mods = if int_like && !mods.is_empty() && !(base == "tinyint" && mods == "(1)") { String::new() } else { mods };
    let mut out = base;
    out.push_str(&mods);
    if !keep_tail.is_empty() {
        out.push(' ');
        out.push_str(&keep_tail);
    }
    for _ in 0..dims {
        out.push_str("[]");
    }
    out
}

/// A name referenced from a SQL fragment.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct NameRef {
    /// Lower-cased qualifier, if the reference was qualified.
    pub schema: Option<String>,
    /// Lower-cased object name.
    pub name: String,
}

/// Names referenced by a SQL fragment.
#[derive(Debug, Clone, Default)]
pub(crate) struct References {
    /// Every identifier (possibly qualified) not followed by `(`.
    pub idents: BTreeSet<NameRef>,
    /// Identifiers followed by `(`: function / procedure calls.
    pub calls: BTreeSet<NameRef>,
    /// Sequences named in `nextval('...')` / `currval` / `setval`.
    pub sequences: BTreeSet<NameRef>,
}

impl References {
    /// True if the fragment references `name` (unqualified, or qualified with
    /// one of `own_schemas`) as a relation-like identifier.
    pub fn mentions(&self, name: &str, own_schemas: &[Option<&str>]) -> bool {
        mentions(&self.idents, name, own_schemas)
    }
    /// True if `name` appears as an identifier with any qualifier (e.g. a
    /// column referenced as `alias.column`).
    pub fn mentions_anywhere(&self, name: &str) -> bool {
        let n = name.to_lowercase();
        self.idents.iter().any(|r| r.name == n)
    }
    pub fn calls(&self, name: &str, own_schemas: &[Option<&str>]) -> bool {
        mentions(&self.calls, name, own_schemas)
    }
    pub fn uses_sequence(&self, name: &str, own_schemas: &[Option<&str>]) -> bool {
        mentions(&self.sequences, name, own_schemas)
    }
}

fn mentions(set: &BTreeSet<NameRef>, name: &str, own_schemas: &[Option<&str>]) -> bool {
    let n = name.to_lowercase();
    set.iter().any(|r| {
        r.name == n
            && match &r.schema {
                None => true,
                Some(s) => own_schemas.iter().flatten().any(|o| o.to_lowercase() == *s),
            }
    })
}

fn qualified_name_from_literal(content: &str) -> Option<NameRef> {
    let mut parts: Vec<String> = Vec::new();
    for part in content.split('.') {
        let p = part.trim();
        let p = p.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(p);
        if p.is_empty() {
            return None;
        }
        parts.push(p.to_lowercase());
    }
    let name = parts.pop()?;
    Some(NameRef { schema: parts.pop(), name })
}

/// Extract names referenced by `text`.
pub(crate) fn references(text: &str) -> References {
    let toks: Vec<(TokKind, &str)> = tokenize(text)
        .into_iter()
        .filter(|t| !matches!(t.kind, TokKind::Space | TokKind::Comment))
        .map(|t| (t.kind, &text[t.start..t.end]))
        .collect();
    let name_of = |k: TokKind, s: &str| -> Option<String> {
        match k {
            TokKind::Word => Some(s.to_lowercase()),
            TokKind::Quoted(c) => Some(unquote(s, c).to_lowercase()),
            _ => None,
        }
    };
    let mut refs = References::default();
    for (k, s) in &toks {
        if *k == TokKind::Str
            && let Some((_, body)) = dollar_body(s) {
                let inner = references(body);
                refs.idents.extend(inner.idents);
                refs.calls.extend(inner.calls);
                refs.sequences.extend(inner.sequences);
            }
    }
    let mut i = 0;
    while i < toks.len() {
        let (k, s) = toks[i];
        let Some(first) = name_of(k, s) else {
            i += 1;
            continue;
        };
        // collect a dotted chain a.b.c
        let mut parts = vec![first];
        let mut j = i + 1;
        while j + 1 < toks.len() && toks[j].0 == TokKind::Punct && toks[j].1 == "." {
            match name_of(toks[j + 1].0, toks[j + 1].1) {
                Some(n) => {
                    parts.push(n);
                    j += 2;
                }
                None => break,
            }
        }
        let is_call = toks.get(j).is_some_and(|(k, s)| *k == TokKind::Punct && *s == "(");
        // record every suffix pair as possible (schema, name) / (table, column)
        let name = parts.last().cloned().unwrap_or_default();
        let schema = if parts.len() >= 2 { Some(parts[parts.len() - 2].clone()) } else { None };
        if is_call {
            refs.calls.insert(NameRef { schema: schema.clone(), name: name.clone() });
            if ["nextval", "currval", "setval"].contains(&name.as_str())
                && let Some((TokKind::Str, lit)) = toks.get(j + 1).copied()
                    && let Some(content) = string_content(lit)
                        && let Some(r) = qualified_name_from_literal(&content) {
                            refs.sequences.insert(r);
                        }
        } else {
            refs.idents.insert(NameRef { schema, name });
            // a.b may also be table.column: record the qualifier as a bare name too
            if parts.len() >= 2 {
                for p in &parts[..parts.len() - 1] {
                    refs.idents.insert(NameRef { schema: None, name: p.clone() });
                }
            }
        }
        // string literals cast to regclass: 'schema.t'::regclass
        i = j.max(i + 1);
    }
    for w in toks.windows(3) {
        if w[0].0 == TokKind::Str && w[1].1 == "::" && w[2].1.eq_ignore_ascii_case("regclass")
            && let Some(content) = string_content(w[0].1)
                && let Some(r) = qualified_name_from_literal(&content) {
                    refs.sequences.insert(r.clone());
                    refs.idents.insert(r);
                }
    }
    refs
}

/// Replace qualifiers equal to `from` with `to` inside SQL text, preserving
/// everything else (whitespace, comments, case). Also rewrites qualified names
/// inside string literals (`nextval('from.seq')`).
pub(crate) fn rename_schema(text: &str, from: &str, to: &str) -> String {
    if from.is_empty() || from == to {
        return text.to_string();
    }
    let toks = tokenize(text);
    let significant: Vec<usize> =
        (0..toks.len()).filter(|&k| !matches!(toks[k].kind, TokKind::Space | TokKind::Comment)).collect();
    let mut replacements: Vec<(usize, usize, String)> = Vec::new();
    for (pos, &k) in significant.iter().enumerate() {
        let t = toks[k];
        let s = &text[t.start..t.end];
        let next_is_dot = significant
            .get(pos + 1)
            .is_some_and(|&n| toks[n].kind == TokKind::Punct && &text[toks[n].start..toks[n].end] == ".");
        let prev_is_dot = pos > 0
            && toks[significant[pos - 1]].kind == TokKind::Punct
            && &text[toks[significant[pos - 1]].start..toks[significant[pos - 1]].end] == ".";
        match t.kind {
            TokKind::Word if next_is_dot && !prev_is_dot && s.eq_ignore_ascii_case(from) => {
                replacements.push((t.start, t.end, to.to_string()));
            }
            TokKind::Quoted(close) if next_is_dot && !prev_is_dot && unquote(s, close) == from => {
                let open = s.chars().next().unwrap_or('"');
                let escaped = to.replace(close, &format!("{close}{close}"));
                replacements.push((t.start, t.end, format!("{open}{escaped}{close}")));
            }
            TokKind::Str if dollar_body(s).is_some() => {
                if let Some((tag, body)) = dollar_body(s) {
                    let renamed = rename_schema(body, from, to);
                    if renamed != body {
                        replacements.push((t.start, t.end, format!("{tag}{renamed}{tag}")));
                    }
                }
            }
            TokKind::Str => {
                if let Some(content) = string_content(s) {
                    let lc = content.to_lowercase();
                    let plain = format!("{}.", from.to_lowercase());
                    let quoted = format!("\"{from}\".");
                    let new_content = if lc.starts_with(&plain) {
                        content.get(plain.len()..).map(|rest| format!("{to}.{rest}"))
                    } else if content.starts_with(&quoted) {
                        content.get(quoted.len()..).map(|rest| format!("\"{to}\".{rest}"))
                    } else {
                        None
                    };
                    if let Some(c) = new_content {
                        replacements.push((t.start, t.end, format!("'{}'", c.replace('\'', "''"))));
                    }
                }
            }
            _ => {}
        }
    }
    if replacements.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut last = 0;
    for (start, end, rep) in replacements {
        out.push_str(&text[last..start]);
        out.push_str(&rep);
        last = end;
    }
    out.push_str(&text[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(s: &str) -> String {
        normalize_sql(s, &SqlNorm::default())
    }

    #[test]
    fn tokenizes_strings_comments_and_quotes() {
        let text = "SELECT 'it''s', \"Col\" -- c\n/* x /* y */ */ $f$ body; $f$ `b` [dbo] arr[1] $1 1.5e3";
        let kinds: Vec<TokKind> =
            tokenize(text).into_iter().map(|t| t.kind).filter(|k| *k != TokKind::Space).collect();
        assert_eq!(
            kinds,
            vec![
                TokKind::Word,
                TokKind::Str,
                TokKind::Punct,
                TokKind::Quoted('"'),
                TokKind::Comment,
                TokKind::Comment,
                TokKind::Str,
                TokKind::Quoted('`'),
                TokKind::Quoted(']'),
                TokKind::Word,
                TokKind::Punct,
                TokKind::Number,
                TokKind::Punct,
                TokKind::Word,
                TokKind::Number,
            ]
        );
    }

    #[test]
    fn normalizes_whitespace_case_quotes_and_comments() {
        assert_eq!(norm("SELECT  id,\n\tName FROM \"users\" -- all\n;"), norm("select id , name from users"));
        assert_ne!(norm("select \"Name\" from t"), norm("select name from t"));
        assert_eq!(norm("select `Name` from t"), norm("select \"Name\" from t"));
        assert_eq!(norm("((price > 0))"), "price > 0");
        assert_eq!(norm("(a) + (b)"), "( a ) + ( b )");
        assert_eq!(norm("'Abc'"), "'Abc'");
    }

    #[test]
    fn strips_own_schema_and_definer() {
        let n = SqlNorm { own_schema: Some("app".into()), ..Default::default() };
        assert_eq!(normalize_sql("select * from app.users u join \"app\".orders o on true", &n), "select * from users u join orders o on true");
        assert_eq!(normalize_sql("nextval('app.users_id_seq'::regclass)", &n), "nextval ( 'users_id_seq' :: regclass )");
        let mysql = "CREATE DEFINER=`root`@`%` PROCEDURE `p`() BEGIN SELECT 1; END";
        assert_eq!(normalize_sql(mysql, &n), normalize_sql("create procedure p() begin select 1; end", &n));
    }

    #[test]
    fn casts_and_functions_for_cross_dialect() {
        let n = SqlNorm { strip_casts: true, canonical_functions: true, ..Default::default() };
        assert_eq!(normalize_sql("'new'::character varying", &n), "'new'");
        assert_eq!(normalize_sql("now()", &n), normalize_sql("CURRENT_TIMESTAMP", &n));
        assert_eq!(normalize_sql("'{}'::jsonb[]", &n), "'{}'");
    }

    #[test]
    fn type_aliases() {
        let t = |s: &str| normalize_type(s, true);
        assert_eq!(t("int4"), t("INTEGER"));
        assert_eq!(t("int(11)"), "integer");
        assert_eq!(t("tinyint(1)"), "tinyint(1)");
        assert_eq!(t("character varying(255)"), "varchar(255)");
        assert_eq!(t("VARCHAR (255)"), "varchar(255)");
        assert_eq!(t("timestamp without time zone"), "timestamp");
        assert_eq!(t("timestamp(3) with time zone"), "timestamptz(3)");
        assert_eq!(t("timestamptz"), t("timestamp with time zone"));
        assert_eq!(t("double precision"), t("float8"));
        assert_eq!(t("numeric(10, 2)"), t("decimal(10,2)"));
        assert_eq!(t("int4[]"), "integer[]");
        assert_eq!(t("int unsigned"), "integer unsigned");
        assert_eq!(t("int(10) unsigned"), "integer unsigned");
        assert_ne!(t("varchar(10)"), t("varchar(20)"));
        assert_eq!(normalize_type("int4", false), "int4");
    }

    #[test]
    fn extracts_references() {
        let r = references("SELECT u.id, calc(o.total) FROM app.users u JOIN orders o ON o.uid = u.id WHERE nextval('app.seq1') > 0");
        assert!(r.mentions("users", &[Some("app")]));
        assert!(r.mentions("orders", &[None]));
        assert!(!r.mentions("users", &[Some("other")]));
        assert!(r.calls("calc", &[None]));
        assert!(r.uses_sequence("seq1", &[Some("app")]));
        let r = references("nextval('\"My\".s2'::regclass)");
        assert!(r.uses_sequence("s2", &[Some("My")]));
    }

    #[test]
    fn dollar_quoted_bodies_are_sql() {
        let n = SqlNorm { own_schema: Some("app".into()), ..Default::default() };
        let a = "CREATE FUNCTION app.f() RETURNS int AS $fn$ SELECT  count(*) FROM app.t $fn$ LANGUAGE sql";
        let b = "create function f() returns int as $$ select count(*) from t $$ language sql";
        assert_eq!(normalize_sql(a, &n), normalize_sql(b, &n));
        assert_eq!(rename_schema(a, "app", "prod"), "CREATE FUNCTION prod.f() RETURNS int AS $fn$ SELECT  count(*) FROM prod.t $fn$ LANGUAGE sql");
        assert!(references(a).mentions("t", &[Some("app")]));
        assert_eq!(dollar_body("$x$ a $x$"), Some(("$x$", " a ")));
        assert_eq!(dollar_body("$$"), None);
    }

    #[test]
    fn renames_schema_qualifiers() {
        let sql = "CREATE VIEW staging.v AS SELECT * FROM staging.users JOIN \"staging\".orders ON true -- staging.x\nWHERE x = 'staging.users' AND nextval('staging.s') > 0";
        let out = rename_schema(sql, "staging", "prod");
        assert_eq!(
            out,
            "CREATE VIEW prod.v AS SELECT * FROM prod.users JOIN \"prod\".orders ON true -- staging.x\nWHERE x = 'prod.users' AND nextval('prod.s') > 0"
        );
        // columns qualified by a table named like the schema are not touched after a dot
        assert_eq!(rename_schema("select a.staging.c", "staging", "prod"), "select a.staging.c");
        assert_eq!(rename_schema("select 1", "a", "b"), "select 1");
    }
}
