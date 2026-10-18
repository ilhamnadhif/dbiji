//! SQL tokenizer.
//!
//! [`tokenize`] splits any text into a gap-free sequence of [`Token`]s: the
//! tokens are contiguous, cover the whole input, and every boundary is a
//! UTF-8 character boundary. It never fails: unterminated strings, quoted
//! identifiers, comments and dollar-quoted bodies run to the end of the input
//! and are flagged with [`Token::unterminated`].
//!
//! Lexical rules come from [`SqlSyntax`]: comment styles, string and
//! identifier quoting, dollar quoting, parameter and variable syntax, the
//! MySQL `DELIMITER` command and batch separators such as T-SQL `GO`.

use crate::lexicon::{WordSet, word_set};
use dbiji_core::SqlSyntax;
use std::borrow::Cow;
use std::rc::Rc;

/// Lexical class of a [`Token`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenKind {
    /// Spaces, tabs and other non line-breaking white space.
    Whitespace,
    /// One line break (`\n`, `\r\n` or `\r`).
    Newline,
    /// `-- ...` or `# ...` up to (not including) the line break.
    LineComment,
    /// `/* ... */`, possibly nested.
    BlockComment,
    /// A word listed in [`SqlSyntax::keywords`] (not directly before or after a `.`).
    Keyword,
    /// Unquoted identifier (any other word).
    Identifier,
    /// `"ident"`, `` `ident` `` or `[ident]` according to [`SqlSyntax::identifier_quotes`].
    QuotedIdentifier,
    /// String literal including prefixes and dollar-quoted bodies: `'a'`,
    /// `E'\n'`, `N'x'`, `X'ff'`, `B'01'`, `$$..$$`, `$tag$..$tag$`, and `"a"`
    /// when [`SqlSyntax::double_quoted_strings`].
    String,
    /// Numeric literal: `1`, `1.5`, `.5`, `1e-3`, `0x1F`, `0b101`, `1_000`.
    Number,
    /// Operator such as `=`, `<>`, `::`, `->>`, `@>`, `||`.
    Operator,
    /// `,`
    Comma,
    /// `;` (a statement terminator unless a custom delimiter is active).
    Semicolon,
    /// `.`
    Dot,
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `[` when brackets are not identifier quotes.
    LBracket,
    /// `]` when brackets are not identifier quotes.
    RBracket,
    /// Bind parameter: `?`, `?1`, `$1`, `:name`.
    Parameter,
    /// Variable: `@var`, `@@var` (when [`SqlSyntax::at_variables`]) and template variables `${name}`.
    Variable,
    /// Client side command line at statement start: `@set x = 1`, `@include f.sql`, `@export {...}`.
    ClientCommand,
    /// A statement separator other than `;`: an active custom delimiter
    /// (`$$`, `//` after `DELIMITER //`), a batch separator line (`GO`), or a
    /// `DELIMITER xx` command line itself (see [`delimiter_command_arg`]).
    Delimiter,
    /// Anything else (stray characters such as `\`).
    Unknown,
}

impl TokenKind {
    /// Whitespace, line breaks and comments.
    #[inline]
    pub fn is_trivia(self) -> bool {
        matches!(
            self,
            TokenKind::Whitespace
                | TokenKind::Newline
                | TokenKind::LineComment
                | TokenKind::BlockComment
        )
    }

    /// Line or block comment.
    #[inline]
    pub fn is_comment(self) -> bool {
        matches!(self, TokenKind::LineComment | TokenKind::BlockComment)
    }

    /// Keyword, identifier or quoted identifier.
    #[inline]
    pub fn is_word(self) -> bool {
        matches!(
            self,
            TokenKind::Keyword | TokenKind::Identifier | TokenKind::QuotedIdentifier
        )
    }
}

/// One token: a kind and a byte range `start..end` into the source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Token {
    /// Lexical class.
    pub kind: TokenKind,
    /// Byte offset of the first character.
    pub start: usize,
    /// Byte offset just past the last character.
    pub end: usize,
    /// The token lacks its closing delimiter and runs to the end of the
    /// input (strings, quoted identifiers, comments, dollar bodies, `${`).
    pub unterminated: bool,
}

impl Token {
    /// A terminated token.
    pub fn new(kind: TokenKind, start: usize, end: usize) -> Token {
        Token {
            kind,
            start,
            end,
            unterminated: false,
        }
    }

    /// Source text of the token (empty if the range does not fit `src`).
    #[inline]
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        src.get(self.start..self.end).unwrap_or("")
    }

    /// Length in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// True for zero-length tokens (never produced by the tokenizer).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }

    /// See [`TokenKind::is_trivia`].
    #[inline]
    pub fn is_trivia(&self) -> bool {
        self.kind.is_trivia()
    }

    /// `start <= offset < end`.
    #[inline]
    pub fn contains(&self, offset: usize) -> bool {
        self.start <= offset && offset < self.end
    }
}

/// Tokenize `src` with the default `;` statement delimiter.
pub fn tokenize(src: &str, syntax: &SqlSyntax) -> Vec<Token> {
    Lexer::new(src, syntax).collect()
}

/// Tokenize `src` with `delimiter` as the initial statement delimiter (as
/// configured by the user; `DELIMITER` commands in the script change it).
/// Occurrences of a delimiter other than `;` become [`TokenKind::Delimiter`].
pub fn tokenize_with_delimiter(src: &str, syntax: &SqlSyntax, delimiter: &str) -> Vec<Token> {
    Lexer::with_delimiter(src, syntax, delimiter).collect()
}

/// For a [`TokenKind::Delimiter`] token produced by a `DELIMITER xx` command
/// line, the new delimiter `xx`; `None` for every other token.
pub fn delimiter_command_arg<'a>(token: &Token, src: &'a str) -> Option<&'a str> {
    if token.kind != TokenKind::Delimiter {
        return None;
    }
    let text = token.text(src);
    let head = text.get(..9)?;
    if !head.eq_ignore_ascii_case("delimiter") {
        return None;
    }
    let rest = &text[9..];
    if !rest.starts_with([' ', '\t']) {
        return None;
    }
    let arg = rest.trim();
    if arg.is_empty() { None } else { Some(arg) }
}

/// Text of a quoted identifier without its quotes, with doubled closing
/// quotes unescaped (`"a""b"` → `a"b`, `[a]]b]` → `a]b`). Unquoted text is
/// returned unchanged.
pub fn unquote_ident(text: &str) -> Cow<'_, str> {
    let mut chars = text.chars();
    let Some(open) = chars.next() else {
        return Cow::Borrowed(text);
    };
    let close = match open {
        '"' => '"',
        '`' => '`',
        '[' => ']',
        _ => return Cow::Borrowed(text),
    };
    let inner = &text[open.len_utf8()..];
    let inner = inner.strip_suffix(close).unwrap_or(inner);
    let doubled: String = [close, close].iter().collect();
    if inner.contains(&doubled) {
        Cow::Owned(inner.replace(&doubled, &close.to_string()))
    } else {
        Cow::Borrowed(inner)
    }
}

/// Multi-character operators recognised by maximal munch (3, then 2 chars).
const OPS3: &[&[u8; 3]] = &[
    b"->>", b"#>>", b"<=>", b"<->", b"!~*", b"!~~", b"~~*", b"||/", b"<<=", b">>=", b"-|-", b"?-|",
    b"?||", b"@@@", b"<<|", b"|>>", b"&<|", b"|&>", b"!!=",
];
const OPS2: &[&[u8; 2]] = &[
    b"<=", b">=", b"<>", b"!=", b"==", b"||", b"&&", b"::", b"->", b"=>", b":=", b"**", b"#>",
    b"#-", b"@>", b"<@", b"@@", b"@?", b"?|", b"?&", b"?#", b"?-", b"<<", b">>", b"!~", b"~*",
    b"~~", b"!!", b"|/", b"^@", b"&<", b"&>", b"+=", b"-=", b"*=", b"/=", b"%=", b"&=", b"^=",
    b"|=", b"!<", b"!>", b"~=", b"<^", b">^",
];

#[inline]
fn is_op_byte(c: u8) -> bool {
    matches!(
        c,
        b'+' | b'-'
            | b'*'
            | b'/'
            | b'%'
            | b'='
            | b'<'
            | b'>'
            | b'!'
            | b'~'
            | b'^'
            | b'&'
            | b'|'
            | b'#'
            | b'@'
            | b'?'
            | b':'
            | b'{'
            | b'}'
    )
}

/// Streaming tokenizer; [`tokenize`] collects it into a vector.
///
/// The lexer tracks a little context so that context dependent constructs
/// are classified correctly: whether it is at the start of a statement
/// (client commands, `DELIMITER`), at the start of a line (`GO`), inside
/// brackets (`a[1:n]` is a slice, not a `:n` parameter) and which statement
/// delimiter is active.
#[derive(Clone)]
pub struct Lexer<'a> {
    src: &'a str,
    b: &'a [u8],
    pos: usize,
    syntax: &'a SqlSyntax,
    keywords: Rc<WordSet>,
    delim: String,
    custom: bool,
    delim_alpha: bool,
    stmt_start: bool,
    line_start: bool,
    line_has_content: bool,
    newline_since_sig: bool,
    blank_since_sig: bool,
    paren_depth: i32,
    bracket_depth: i32,
    prev_sig: Option<(TokenKind, usize)>,
    dq_ident: bool,
    bt_ident: bool,
    br_ident: bool,
}

impl<'a> Lexer<'a> {
    /// Lexer with the default `;` delimiter.
    pub fn new(src: &'a str, syntax: &'a SqlSyntax) -> Lexer<'a> {
        Lexer::with_delimiter(src, syntax, ";")
    }

    /// Lexer with a custom initial delimiter (empty means `;`).
    pub fn with_delimiter(src: &'a str, syntax: &'a SqlSyntax, delimiter: &str) -> Lexer<'a> {
        let quotes = syntax.identifier_quotes;
        let mut lx = Lexer {
            src,
            b: src.as_bytes(),
            pos: 0,
            syntax,
            keywords: word_set(syntax.keywords, false),
            delim: String::new(),
            custom: false,
            delim_alpha: false,
            stmt_start: true,
            line_start: true,
            line_has_content: false,
            newline_since_sig: false,
            blank_since_sig: false,
            paren_depth: 0,
            bracket_depth: 0,
            prev_sig: None,
            dq_ident: quotes.iter().any(|q| q.0 == '"'),
            bt_ident: quotes.iter().any(|q| q.0 == '`'),
            br_ident: quotes.iter().any(|q| q.0 == '['),
        };
        lx.set_delimiter(delimiter);
        lx
    }

    /// The statement delimiter currently in effect.
    pub fn active_delimiter(&self) -> &str {
        &self.delim
    }

    fn set_delimiter(&mut self, d: &str) {
        let d = d.trim();
        let d = if d.is_empty() { ";" } else { d };
        self.delim = d.to_string();
        self.custom = d != ";";
        self.delim_alpha = d.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_');
    }

    #[inline]
    fn byte(&self, i: usize) -> u8 {
        self.b.get(i).copied().unwrap_or(0)
    }

    /// End of the character starting at `i`.
    #[inline]
    fn char_end(&self, i: usize) -> usize {
        let mut j = i + 1;
        while j < self.b.len() && !self.src.is_char_boundary(j) {
            j += 1;
        }
        j.min(self.b.len())
    }

    #[inline]
    fn char_at(&self, i: usize) -> Option<char> {
        self.src.get(i..).and_then(|s| s.chars().next())
    }

    fn line_end(&self, mut i: usize) -> usize {
        while i < self.b.len() && self.b[i] != b'\n' && self.b[i] != b'\r' {
            i += 1;
        }
        i
    }

    /// Identifier character test with an ASCII fast path.
    #[inline]
    fn ident_char(&self, i: usize, first: bool) -> Option<usize> {
        let c = self.byte(i);
        if i >= self.b.len() {
            return None;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            return Some(i + 1);
        }
        if c.is_ascii_digit() {
            return if first { None } else { Some(i + 1) };
        }
        if c == b'$' && self.byte(i + 1) == b'{' {
            // `${name}` template variable glued to an identifier.
            return None;
        }
        if c < 0x80 {
            return if self.syntax.is_ident_char(c as char, first) {
                Some(i + 1)
            } else {
                None
            };
        }
        let ch = self.char_at(i)?;
        if self.syntax.is_ident_char(ch, first) {
            Some(i + ch.len_utf8())
        } else {
            None
        }
    }

    #[inline]
    fn is_ident_start(&self, i: usize) -> bool {
        self.ident_char(i, true).is_some()
    }

    fn delim_matches(&self, i: usize) -> bool {
        let d = self.delim.as_bytes();
        if i + d.len() > self.b.len() {
            return false;
        }
        let hay = &self.b[i..i + d.len()];
        if self.delim_alpha {
            if !hay.eq_ignore_ascii_case(d) {
                return false;
            }
            let before_ok =
                i == 0 || !(self.b[i - 1].is_ascii_alphanumeric() || self.b[i - 1] == b'_');
            let after = self.byte(i + d.len());
            let after_ok = !(after.is_ascii_alphanumeric() || after == b'_');
            before_ok && after_ok
        } else {
            hay == d
        }
    }

    /// Scan a word starting at `start` (the first char is known to be valid).
    fn scan_word(&self, start: usize) -> usize {
        let mut i = match self.ident_char(start, true) {
            Some(e) => e,
            None => self.char_end(start),
        };
        while i < self.b.len() {
            if self.custom && self.delim_matches(i) {
                break;
            }
            match self.ident_char(i, false) {
                Some(e) => i = e,
                None => break,
            }
        }
        i
    }

    /// Continue an identifier (no first-char restriction) from `i`.
    fn scan_ident_rest(&self, mut i: usize) -> usize {
        while i < self.b.len() {
            if self.custom && self.delim_matches(i) {
                break;
            }
            match self.ident_char(i, false) {
                Some(e) => i = e,
                None => break,
            }
        }
        i
    }

    fn command_position(&self) -> bool {
        self.stmt_start || (self.line_start && self.blank_since_sig && self.paren_depth <= 0)
    }

    fn scan_quoted(&self, mut i: usize, close: u8, backslash: bool) -> (usize, bool) {
        let len = self.b.len();
        while i < len {
            let c = self.b[i];
            if backslash && c == b'\\' {
                i += 2;
                continue;
            }
            if c == close {
                if i + 1 < len && self.b[i + 1] == close {
                    i += 2;
                    continue;
                }
                return (i + 1, false);
            }
            i += 1;
        }
        (len, true)
    }

    fn scan_block_comment(&self, start: usize) -> (usize, bool) {
        let nested = self.syntax.nested_block_comments;
        let len = self.b.len();
        let mut depth = 1u32;
        let mut i = start + 2;
        while i < len {
            let c = self.b[i];
            if c == b'*' && self.byte(i + 1) == b'/' {
                depth -= 1;
                i += 2;
                if depth == 0 {
                    return (i, false);
                }
                continue;
            }
            if nested && c == b'/' && self.byte(i + 1) == b'*' {
                depth += 1;
                i += 2;
                continue;
            }
            i += 1;
        }
        (len, true)
    }

    /// `$tag$` (or `$$`) starting at `start`: end of the opening tag.
    fn dollar_tag(&self, start: usize) -> Option<usize> {
        let mut i = start + 1;
        if self.byte(i) == b'$' {
            return Some(i + 1);
        }
        // Tags follow identifier rules (letter or `_` first) but cannot contain `$`.
        let mut first = true;
        loop {
            if i >= self.b.len() {
                return None;
            }
            let c = self.b[i];
            if c == b'$' {
                return if first { None } else { Some(i + 1) };
            }
            if c.is_ascii_alphabetic() || c == b'_' || (!first && c.is_ascii_digit()) {
                i += 1;
            } else if c >= 0x80 {
                let ch = self.char_at(i)?;
                if !(ch.is_alphabetic() || (!first && ch.is_alphanumeric())) {
                    return None;
                }
                i += ch.len_utf8();
            } else {
                return None;
            }
            first = false;
        }
    }

    fn scan_number(&self, start: usize) -> (TokenKind, usize) {
        let b = self.b;
        let len = b.len();
        let digits = |mut i: usize, hex: bool| {
            while i < len {
                let c = b[i];
                let ok = if hex {
                    c.is_ascii_hexdigit()
                } else {
                    c.is_ascii_digit()
                };
                if ok
                    || (c == b'_'
                        && i + 1 < len
                        && (if hex {
                            b[i + 1].is_ascii_hexdigit()
                        } else {
                            b[i + 1].is_ascii_digit()
                        }))
                {
                    i += 1;
                } else {
                    break;
                }
            }
            i
        };
        let mut i = start;
        let c0 = b[start];
        let c1 = self.byte(start + 1) | 0x20;
        if c0 == b'0' && c1 == b'x' && self.byte(start + 2).is_ascii_hexdigit() {
            i = digits(start + 2, true);
        } else if c0 == b'0' && c1 == b'b' && matches!(self.byte(start + 2), b'0' | b'1') {
            i = start + 2;
            while i < len && matches!(b[i], b'0' | b'1') {
                i += 1;
            }
        } else {
            if c0 != b'.' {
                i = digits(start, false);
            }
            if i < len && b[i] == b'.' && self.byte(i + 1) != b'.' {
                i = digits(i + 1, false);
            }
            if i < len && (b[i] | 0x20) == b'e' {
                let mut j = i + 1;
                if matches!(self.byte(j), b'+' | b'-') {
                    j += 1;
                }
                if self.byte(j).is_ascii_digit() {
                    i = digits(j, false);
                }
            }
        }
        // MySQL allows identifiers that start with digits (`1abc`).
        if i < len && !(self.custom && self.delim_matches(i)) {
            let c = b[i];
            if c.is_ascii_alphabetic()
                || c == b'_'
                || (c >= 0x80 && self.ident_char(i, false).is_some())
            {
                return (TokenKind::Identifier, self.scan_ident_rest(i));
            }
        }
        (TokenKind::Number, i.max(start + 1))
    }

    fn whitespace_run(&self, mut i: usize) -> usize {
        while i < self.b.len() {
            let c = self.b[i];
            match c {
                b' ' | b'\t' | 0x0b | 0x0c => i += 1,
                0x80.. => match self.char_at(i) {
                    Some(ch) if ch.is_whitespace() => i += ch.len_utf8(),
                    _ => break,
                },
                _ => break,
            }
        }
        i
    }

    fn try_batch_separator(&self, start: usize) -> Option<usize> {
        let sep = self.syntax.batch_separator?.as_bytes();
        if sep.is_empty() || start + sep.len() > self.b.len() {
            return None;
        }
        if !self.b[start..start + sep.len()].eq_ignore_ascii_case(sep) {
            return None;
        }
        let mut i = start + sep.len();
        let c = self.byte(i);
        if c.is_ascii_alphanumeric()
            || c == b'_'
            || (c >= 0x80 && self.ident_char(i, false).is_some())
        {
            return None;
        }
        let mut end = i;
        while matches!(self.byte(i), b' ' | b'\t') {
            i += 1;
        }
        if self.byte(i).is_ascii_digit() {
            while self.byte(i).is_ascii_digit() {
                i += 1;
            }
            end = i;
            while matches!(self.byte(i), b' ' | b'\t') {
                i += 1;
            }
        }
        let rest_ok = i >= self.b.len()
            || matches!(self.b[i], b'\n' | b'\r')
            || (self.b[i] == b'-' && self.byte(i + 1) == b'-');
        if rest_ok { Some(end) } else { None }
    }

    fn try_delimiter_command(&self, start: usize) -> Option<(usize, usize)> {
        const KW: &[u8] = b"delimiter";
        if start + KW.len() >= self.b.len()
            || !self.b[start..start + KW.len()].eq_ignore_ascii_case(KW)
        {
            return None;
        }
        let mut i = start + KW.len();
        if !matches!(self.byte(i), b' ' | b'\t') {
            return None;
        }
        while matches!(self.byte(i), b' ' | b'\t') {
            i += 1;
        }
        let arg_start = i;
        while i < self.b.len() && !matches!(self.b[i], b' ' | b'\t' | b'\n' | b'\r') {
            i += 1;
        }
        // `i` stops at ASCII white space or the end, both char boundaries.
        if i == arg_start {
            None
        } else {
            Some((arg_start, i))
        }
    }

    fn scan_client_command(&self, start: usize) -> usize {
        let len = self.b.len();
        let mut i = start + 1;
        while i < len && (self.b[i].is_ascii_alphanumeric() || self.b[i] == b'_') {
            i += 1;
        }
        while matches!(self.byte(i), b' ' | b'\t') {
            i += 1;
        }
        let mut end = if self.byte(i) == b'{' {
            // JSON argument: continue across lines until the braces balance.
            let mut depth = 0i32;
            let mut in_str = false;
            let mut j = i;
            while j < len {
                let c = self.b[j];
                if in_str {
                    if c == b'\\' {
                        j += 2;
                        continue;
                    }
                    if c == b'"' {
                        in_str = false;
                    }
                } else {
                    match c {
                        b'"' => in_str = true,
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                j += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                j += 1;
            }
            self.line_end(j.min(len))
        } else {
            self.line_end(start)
        };
        while end > start + 1 && matches!(self.b[end - 1], b' ' | b'\t') {
            end -= 1;
        }
        end
    }

    fn qualifier_before(&self, start: usize) -> bool {
        matches!(
            self.prev_sig,
            Some((
                TokenKind::Identifier
                    | TokenKind::QuotedIdentifier
                    | TokenKind::Keyword
                    | TokenKind::RParen
                    | TokenKind::RBracket
                    | TokenKind::Variable
                    | TokenKind::Parameter,
                end
            )) if end == start
        )
    }

    fn op_len(&self, start: usize) -> usize {
        let rest = &self.b[start..];
        let fits = |n: usize| !self.custom || !(1..n).any(|k| self.delim_matches(start + k));
        if rest.len() >= 3 && fits(3) && OPS3.iter().any(|op| rest[..3] == op[..]) {
            return 3;
        }
        if rest.len() >= 2 && fits(2) && OPS2.iter().any(|op| rest[..2] == op[..]) {
            return 2;
        }
        1
    }

    /// Lex one token at `start`; returns (kind, end, unterminated).
    fn lex(&mut self, start: usize) -> (TokenKind, usize, bool) {
        use TokenKind as K;
        let b = self.b;
        let len = b.len();
        let c = b[start];
        match c {
            b'\n' => return (K::Newline, start + 1, false),
            b'\r' => {
                return (
                    K::Newline,
                    if self.byte(start + 1) == b'\n' {
                        start + 2
                    } else {
                        start + 1
                    },
                    false,
                );
            }
            b' ' | b'\t' | 0x0b | 0x0c => {
                return (K::Whitespace, self.whitespace_run(start), false);
            }
            _ => {}
        }
        if self.line_start {
            if let Some(end) = self.try_batch_separator(start) {
                return (K::Delimiter, end, false);
            }
            if self.syntax.delimiter_command
                && self.command_position()
                && let Some((a, e)) = self.try_delimiter_command(start)
            {
                let arg = self.src[a..e].to_string();
                self.set_delimiter(&arg);
                return (K::Delimiter, e, false);
            }
        }
        if c == b'@'
            && self.byte(start + 1).is_ascii_alphabetic()
            && (self.stmt_start || self.command_position())
        {
            return (K::ClientCommand, self.scan_client_command(start), false);
        }
        if self.custom && self.delim_matches(start) {
            return (K::Delimiter, start + self.delim.len(), false);
        }
        let next = self.byte(start + 1);
        match c {
            b'-' if next == b'-' => {
                let k = start + 2;
                if !self.syntax.dash_comment_needs_space || k >= len || b[k] <= b' ' {
                    return (K::LineComment, self.line_end(start), false);
                }
            }
            b'#' if self.syntax.hash_comments => {
                return (K::LineComment, self.line_end(start), false);
            }
            b'/' if next == b'*' => {
                let (e, u) = self.scan_block_comment(start);
                return (K::BlockComment, e, u);
            }
            b'\'' => {
                let (e, u) = self.scan_quoted(start + 1, b'\'', self.syntax.backslash_escapes);
                return (K::String, e, u);
            }
            b'"' => {
                if self.syntax.double_quoted_strings {
                    let (e, u) = self.scan_quoted(start + 1, b'"', self.syntax.backslash_escapes);
                    return (K::String, e, u);
                }
                if self.dq_ident {
                    let (e, u) = self.scan_quoted(start + 1, b'"', false);
                    return (K::QuotedIdentifier, e, u);
                }
                return (K::Unknown, start + 1, false);
            }
            b'`' if self.bt_ident => {
                let (e, u) = self.scan_quoted(start + 1, b'`', false);
                return (K::QuotedIdentifier, e, u);
            }
            b'[' if self.br_ident => {
                let (e, u) = self.scan_quoted(start + 1, b']', false);
                return (K::QuotedIdentifier, e, u);
            }
            b'$' => {
                if next == b'{' {
                    let mut i = start + 2;
                    while i < len
                        && (b[i].is_ascii_alphanumeric() || matches!(b[i], b'_' | b'.' | b'-'))
                    {
                        i += 1;
                    }
                    if self.byte(i) == b'}' {
                        return (K::Variable, i + 1, false);
                    }
                    return (K::Variable, i, true);
                }
                if self.syntax.dollar_quoting
                    && let Some(tag_end) = self.dollar_tag(start)
                {
                    let tag = &self.src[start..tag_end];
                    return match self.src[tag_end..].find(tag) {
                        Some(p) => (K::String, tag_end + p + tag.len(), false),
                        None => (K::String, len, true),
                    };
                }
                if self.syntax.dollar_params && next.is_ascii_digit() {
                    let mut i = start + 1;
                    while self.byte(i).is_ascii_digit() {
                        i += 1;
                    }
                    return (K::Parameter, i, false);
                }
                return (K::Unknown, start + 1, false);
            }
            b'0'..=b'9' => {
                let (k, e) = self.scan_number(start);
                return (k, e, false);
            }
            b'.' => {
                if next.is_ascii_digit() && !self.qualifier_before(start) {
                    let (k, e) = self.scan_number(start);
                    return (k, e, false);
                }
                return (K::Dot, start + 1, false);
            }
            b'@' if self.syntax.at_variables => {
                let mut i = start + 1;
                if self.byte(i) == b'@' {
                    i += 1;
                }
                let q = self.byte(i);
                if matches!(q, b'\'' | b'"' | b'`') {
                    let (e, u) = self.scan_quoted(i + 1, q, false);
                    return (K::Variable, e, u);
                }
                let e = self.scan_ident_rest(i);
                if e > i {
                    return (K::Variable, e, false);
                }
                return (K::Operator, i, false);
            }
            b':' => {
                if next == b':' || next == b'=' {
                    return (K::Operator, start + 2, false);
                }
                if self.bracket_depth <= 0 && self.is_ident_start(start + 1) {
                    let prev = if start == 0 { 0 } else { b[start - 1] };
                    let prev_ok = !(prev.is_ascii_alphanumeric()
                        || prev >= 0x80
                        || matches!(prev, b'_' | b'$' | b':' | b'"' | b'`' | b']' | b')' | b'\''));
                    if prev_ok {
                        return (K::Parameter, self.scan_word(start + 1), false);
                    }
                }
                return (K::Operator, start + 1, false);
            }
            b'?' => {
                if matches!(next, b'|' | b'&' | b'#' | b'-') {
                    return (K::Operator, start + self.op_len(start), false);
                }
                if self.syntax.question_params {
                    let mut i = start + 1;
                    while self.byte(i).is_ascii_digit() {
                        i += 1;
                    }
                    return (K::Parameter, i, false);
                }
                return (K::Operator, start + 1, false);
            }
            b',' => return (K::Comma, start + 1, false),
            b';' => return (K::Semicolon, start + 1, false),
            b'(' => return (K::LParen, start + 1, false),
            b')' => return (K::RParen, start + 1, false),
            b'[' => return (K::LBracket, start + 1, false),
            b']' => return (K::RBracket, start + 1, false),
            _ => {}
        }
        // Prefixed string literals: E'..', N'..', X'..', B'..', U&'..'.
        if next == b'\'' {
            match c | 0x20 {
                b'e' if self.syntax.e_strings => {
                    let (e, u) = self.scan_quoted(start + 2, b'\'', true);
                    return (K::String, e, u);
                }
                b'n' if self.syntax.n_strings => {
                    let (e, u) = self.scan_quoted(start + 2, b'\'', self.syntax.backslash_escapes);
                    return (K::String, e, u);
                }
                b'x' if self.syntax.x_strings => {
                    let (e, u) = self.scan_quoted(start + 2, b'\'', false);
                    return (K::String, e, u);
                }
                b'b' => {
                    let (e, u) = self.scan_quoted(start + 2, b'\'', false);
                    return (K::String, e, u);
                }
                _ => {}
            }
        }
        if (c | 0x20) == b'u' && next == b'&' && self.syntax.e_strings {
            match self.byte(start + 2) {
                b'\'' => {
                    let (e, u) = self.scan_quoted(start + 3, b'\'', false);
                    return (K::String, e, u);
                }
                b'"' => {
                    let (e, u) = self.scan_quoted(start + 3, b'"', false);
                    return (K::QuotedIdentifier, e, u);
                }
                _ => {}
            }
        }
        if self.is_ident_start(start) {
            let end = self.scan_word(start);
            let text = &self.src[start..end];
            let after_dot = matches!(self.prev_sig, Some((K::Dot, _)));
            let before_dot = self.byte(end) == b'.' && {
                let n = self.byte(end + 1);
                n == b'*' || n == b'"' || n == b'`' || n == b'[' || self.is_ident_start(end + 1)
            };
            let kind = if !after_dot && !before_dot && self.keywords.contains(text) {
                K::Keyword
            } else {
                K::Identifier
            };
            return (kind, end, false);
        }
        if c >= 0x80 {
            let e = self.char_end(start);
            if self.char_at(start).is_some_and(char::is_whitespace) {
                return (K::Whitespace, self.whitespace_run(start), false);
            }
            return (K::Unknown, e, false);
        }
        if is_op_byte(c) {
            return (K::Operator, start + self.op_len(start), false);
        }
        (K::Unknown, start + 1, false)
    }

    fn after(&mut self, kind: TokenKind, end: usize) {
        use TokenKind as K;
        match kind {
            K::Newline => {
                if !self.line_has_content && self.newline_since_sig {
                    self.blank_since_sig = true;
                }
                self.newline_since_sig = true;
                self.line_has_content = false;
                self.line_start = true;
            }
            K::Whitespace => {}
            K::LineComment | K::BlockComment => {
                self.line_has_content = true;
                self.line_start = false;
            }
            K::Delimiter | K::ClientCommand => self.terminate(kind, end),
            K::Semicolon if !self.custom => self.terminate(kind, end),
            _ => {
                self.stmt_start = false;
                self.line_start = false;
                self.line_has_content = true;
                self.newline_since_sig = false;
                self.blank_since_sig = false;
                self.prev_sig = Some((kind, end));
                match kind {
                    K::LParen => self.paren_depth += 1,
                    K::RParen => self.paren_depth -= 1,
                    K::LBracket => self.bracket_depth += 1,
                    K::RBracket => self.bracket_depth -= 1,
                    _ => {}
                }
            }
        }
    }

    fn terminate(&mut self, kind: TokenKind, end: usize) {
        self.stmt_start = true;
        self.line_start = false;
        self.line_has_content = true;
        self.newline_since_sig = false;
        self.blank_since_sig = false;
        self.paren_depth = 0;
        self.bracket_depth = 0;
        self.prev_sig = Some((kind, end));
    }
}

impl Iterator for Lexer<'_> {
    type Item = Token;

    fn next(&mut self) -> Option<Token> {
        let start = self.pos;
        if start >= self.b.len() {
            return None;
        }
        let (kind, mut end, unterminated) = self.lex(start);
        // Defensive: always make progress and stop on a char boundary.
        if end <= start {
            end = self.char_end(start);
        }
        end = end.min(self.b.len());
        while end < self.b.len() && !self.src.is_char_boundary(end) {
            end += 1;
        }
        self.pos = end;
        self.after(kind, end);
        Some(Token {
            kind,
            start,
            end,
            unterminated,
        })
    }
}

/// Word token (keyword or identifier) whose text equals `word` ignoring ASCII case.
#[inline]
pub(crate) fn is_word(t: &Token, src: &str, word: &str) -> bool {
    matches!(t.kind, TokenKind::Keyword | TokenKind::Identifier)
        && t.len() == word.len()
        && t.text(src).eq_ignore_ascii_case(word)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use TokenKind as K;

    fn kinds(src: &str, syn: &SqlSyntax) -> Vec<(K, String)> {
        tokenize(src, syn)
            .into_iter()
            .filter(|t| !t.is_trivia() || t.kind.is_comment())
            .map(|t| (t.kind, t.text(src).to_string()))
            .collect()
    }

    fn check_cover(src: &str, toks: &[Token]) {
        let mut pos = 0;
        for t in toks {
            assert_eq!(t.start, pos, "gap before {t:?} in {src:?}");
            assert!(t.end > t.start, "empty token {t:?}");
            assert!(src.is_char_boundary(t.start) && src.is_char_boundary(t.end));
            pos = t.end;
        }
        assert_eq!(pos, src.len());
    }

    #[test]
    fn basic_select() {
        let src = "SELECT a, b FROM t WHERE x = 1;";
        let k = kinds(src, &pg());
        assert_eq!(
            k,
            vec![
                (K::Keyword, "SELECT".into()),
                (K::Identifier, "a".into()),
                (K::Comma, ",".into()),
                (K::Identifier, "b".into()),
                (K::Keyword, "FROM".into()),
                (K::Identifier, "t".into()),
                (K::Keyword, "WHERE".into()),
                (K::Identifier, "x".into()),
                (K::Operator, "=".into()),
                (K::Number, "1".into()),
                (K::Semicolon, ";".into()),
            ]
        );
        check_cover(src, &tokenize(src, &pg()));
    }

    #[test]
    fn keywords_are_case_insensitive_and_not_after_dot() {
        let k = kinds("select u.select, user.id from users", &pg());
        assert_eq!(k[0].0, K::Keyword);
        assert_eq!(k[1], (K::Identifier, "u".into()));
        assert_eq!(k[3], (K::Identifier, "select".into()));
        assert_eq!(k[5], (K::Identifier, "user".into()));
        assert_eq!(k[8].0, K::Keyword);
    }

    #[test]
    fn comments() {
        let k = kinds("a -- c\nb /* x /* y */ z */ c", &pg());
        assert_eq!(k[1], (K::LineComment, "-- c".into()));
        assert_eq!(k[3], (K::BlockComment, "/* x /* y */ z */".into()));
        assert_eq!(k[4], (K::Identifier, "c".into()));
        // Not nested in MySQL
        let k = kinds("/* x /* y */ z */", &mysql());
        assert_eq!(k[0], (K::BlockComment, "/* x /* y */".into()));
        // Hash comments only in MySQL
        let k = kinds("a # c\nb", &mysql());
        assert_eq!(k[1], (K::LineComment, "# c".into()));
        let k = kinds("a # b", &pg());
        assert_eq!(k[1], (K::Operator, "#".into()));
        // Unterminated block comment runs to EOF
        let t = tokenize("a /* open", &pg());
        assert_eq!(
            t.last().map(|t| (t.kind, t.unterminated)),
            Some((K::BlockComment, true))
        );
    }

    #[test]
    fn mysql_dash_comment_needs_space() {
        let k = kinds("SELECT 1--1", &mysql());
        assert_eq!(
            k.iter().map(|x| x.1.as_str()).collect::<Vec<_>>(),
            vec!["SELECT", "1", "-", "-", "1"]
        );
        let k = kinds("SELECT 1 -- c", &mysql());
        assert_eq!(k[2], (K::LineComment, "-- c".into()));
        let k = kinds("SELECT 1 --", &mysql());
        assert_eq!(k[2], (K::LineComment, "--".into()));
        let k = kinds("SELECT 1--1", &pg());
        assert_eq!(k[2], (K::LineComment, "--1".into()));
    }

    #[test]
    fn strings() {
        let k = kinds(
            "'it''s' 'a\\'b' E'x\\'y' N'n' X'ff' B'01' U&'d\\0061'",
            &pg(),
        );
        assert_eq!(k[0], (K::String, "'it''s'".into()));
        // No backslash escapes in standard PG strings: 'a\' ends at the second quote
        assert_eq!(k[1], (K::String, "'a\\'".into()));
        let k = kinds("E'x\\'y' X'ff' B'01' U&'d\\0061'", &pg());
        assert_eq!(k[0], (K::String, "E'x\\'y'".into()));
        assert_eq!(k[1], (K::String, "X'ff'".into()));
        assert_eq!(k[2], (K::String, "B'01'".into()));
        assert_eq!(k[3], (K::String, "U&'d\\0061'".into()));
        let k = kinds("'a\\'b' \"dq\\\"x\" N'n'", &mysql());
        assert_eq!(k[0], (K::String, "'a\\'b'".into()));
        assert_eq!(k[1], (K::String, "\"dq\\\"x\"".into()));
        assert_eq!(k[2], (K::String, "N'n'".into()));
        let t = tokenize("'open", &pg());
        assert!(t[0].unterminated && t[0].kind == K::String && t[0].end == 5);
    }

    #[test]
    fn quoted_identifiers() {
        let k = kinds("\"My \"\"T\"\" \".x `a``b` [c]]d]", &pg());
        assert_eq!(k[0], (K::QuotedIdentifier, "\"My \"\"T\"\" \"".into()));
        assert_eq!(k[1].0, K::Dot);
        let k = kinds("`a``b`.c", &mysql());
        assert_eq!(k[0], (K::QuotedIdentifier, "`a``b`".into()));
        let k = kinds("[c]]d].[x]", &tsql());
        assert_eq!(k[0], (K::QuotedIdentifier, "[c]]d]".into()));
        assert_eq!(k[2], (K::QuotedIdentifier, "[x]".into()));
        assert_eq!(unquote_ident("\"a\"\"b\""), "a\"b");
        assert_eq!(unquote_ident("[a]]b]"), "a]b");
        assert_eq!(unquote_ident("`a``b`"), "a`b");
        assert_eq!(unquote_ident("plain"), "plain");
        assert_eq!(unquote_ident("\"open"), "open");
        // Brackets are not quotes in PG
        let k = kinds("a[1]", &pg());
        assert_eq!(
            k.iter().map(|x| x.0).collect::<Vec<_>>(),
            vec![K::Identifier, K::LBracket, K::Number, K::RBracket]
        );
    }

    #[test]
    fn dollar_quoting() {
        let src = "DO $$ BEGIN RAISE 'x;'; END $$; SELECT $fn$ a $$ b $fn$, $1";
        let k = kinds(src, &pg());
        assert_eq!(k[1], (K::String, "$$ BEGIN RAISE 'x;'; END $$".into()));
        assert_eq!(k[2].0, K::Semicolon);
        assert_eq!(k[4], (K::String, "$fn$ a $$ b $fn$".into()));
        assert_eq!(k[6], (K::Parameter, "$1".into()));
        let t = tokenize("SELECT $x$ open", &pg());
        assert!(
            t.last()
                .is_some_and(|t| t.kind == K::String && t.unterminated)
        );
        // Identifiers may contain `$` in PG
        let k = kinds("a$b$ c", &pg());
        assert_eq!(k[0], (K::Identifier, "a$b$".into()));
        // No dollar quoting in MySQL
        let k = kinds("$$", &mysql());
        assert_eq!(k[0].0, K::Unknown);
    }

    #[test]
    fn numbers() {
        let k = kinds("1 1.5 .5 1e10 1.5E-3 0x1F 0b101 1_000 1. 2abc", &pg());
        let texts: Vec<_> = k.iter().map(|x| (x.0, x.1.as_str())).collect();
        assert_eq!(
            texts,
            vec![
                (K::Number, "1"),
                (K::Number, "1.5"),
                (K::Number, ".5"),
                (K::Number, "1e10"),
                (K::Number, "1.5E-3"),
                (K::Number, "0x1F"),
                (K::Number, "0b101"),
                (K::Number, "1_000"),
                (K::Number, "1."),
                (K::Identifier, "2abc"),
            ]
        );
        let k = kinds("t.5", &pg());
        assert_eq!(k[1].0, K::Dot);
        let k = kinds("1..10", &pg());
        assert_eq!(k[0], (K::Number, "1".into()));
    }

    #[test]
    fn parameters() {
        let k = kinds(
            "a = :name AND b::int = ? AND c := 1 AND x[1:2] AND y[1:n] AND f(:p)",
            &mysql(),
        );
        let params: Vec<_> = k
            .iter()
            .filter(|x| x.0 == K::Parameter)
            .map(|x| x.1.as_str())
            .collect();
        assert_eq!(params, vec![":name", "?", ":p"]);
        assert!(k.iter().any(|x| x == &(K::Operator, "::".into())));
        assert!(k.iter().any(|x| x == &(K::Operator, ":=".into())));
        // PG: `?` is an operator, `?|` too, `$1` a parameter
        let k = kinds("d ? 'a' AND d ?| b AND $2", &pg());
        assert_eq!(k[1], (K::Operator, "?".into()));
        assert_eq!(k[5], (K::Operator, "?|".into()));
        assert_eq!(k[8], (K::Parameter, "$2".into()));
        // `ident:x` is a path accessor, not a parameter
        let k = kinds("col:field", &pg());
        assert!(k.iter().all(|x| x.0 != K::Parameter));
    }

    #[test]
    fn templates_and_variables() {
        let k = kinds("SELECT ${tbl}, ${open", &pg());
        assert_eq!(k[1], (K::Variable, "${tbl}".into()));
        assert_eq!(k[3], (K::Variable, "${open".into()));
        let k = kinds("SELECT * FROM log_${suffix}", &pg());
        assert_eq!(k[3], (K::Identifier, "log_".into()));
        assert_eq!(k[4], (K::Variable, "${suffix}".into()));
        let k = kinds("SELECT @x, @@version, @`q v`", &mysql());
        assert_eq!(k[1], (K::Variable, "@x".into()));
        assert_eq!(k[3], (K::Variable, "@@version".into()));
        assert_eq!(k[5], (K::Variable, "@`q v`".into()));
        let k = kinds("a @> b AND c <@ d AND e @@ f", &pg());
        assert_eq!(k[1], (K::Operator, "@>".into()));
        assert_eq!(k[5], (K::Operator, "<@".into()));
        assert_eq!(k[9], (K::Operator, "@@".into()));
        let k = kinds("DECLARE @x INT; SELECT #tmp.a", &tsql());
        assert_eq!(k[1], (K::Variable, "@x".into()));
        assert_eq!(k[5], (K::Identifier, "#tmp".into()));
    }

    #[test]
    fn client_commands() {
        let src = "@set x = 1\nSELECT ${x};\n  @include f.sql\nSELECT 1; @echo hi";
        let k = kinds(src, &pg());
        assert_eq!(k[0], (K::ClientCommand, "@set x = 1".into()));
        assert!(k.contains(&(K::ClientCommand, "@include f.sql".into())));
        assert!(k.contains(&(K::ClientCommand, "@echo hi".into())));
        // Not at statement start: an operator in PG, a variable in MySQL
        let k = kinds("SELECT 1\n@x", &mysql());
        assert_eq!(k[2], (K::Variable, "@x".into()));
        // After a blank line it starts a new statement
        let k = kinds("SELECT 1\n\n@set y = 2", &mysql());
        assert_eq!(k[2], (K::ClientCommand, "@set y = 2".into()));
        // Multi-line JSON export
        let src = "@export {\"type\": \"csv\",\n \"x\": {\"a\": \"}\"}}\nSELECT 1";
        let k = kinds(src, &pg());
        assert_eq!(k[0].0, K::ClientCommand);
        assert!(k[0].1.ends_with("}}"), "{:?}", k[0]);
        assert_eq!(k[1], (K::Keyword, "SELECT".into()));
    }

    #[test]
    fn operators() {
        let src = "a<=b>=c<>d!=e||f::g->h->>i#>j#>>k@>l<@m:=n=>o**p?|q?&r";
        let k = kinds(src, &pg());
        let ops: Vec<_> = k
            .iter()
            .filter(|x| x.0 == K::Operator)
            .map(|x| x.1.as_str())
            .collect();
        assert_eq!(
            ops,
            vec![
                "<=", ">=", "<>", "!=", "||", "::", "->", "->>", "#>", "#>>", "@>", "<@", ":=",
                "=>", "**", "?|", "?&"
            ]
        );
        let k = kinds("a<=>b", &mysql());
        assert_eq!(k[1], (K::Operator, "<=>".into()));
    }

    #[test]
    fn delimiter_command() {
        let src =
            "DELIMITER //\nCREATE PROCEDURE p() BEGIN SELECT 1; END//\nDELIMITER ;\nSELECT 2;";
        let t = tokenize(src, &mysql());
        check_cover(src, &t);
        let d: Vec<_> = t
            .iter()
            .filter(|t| t.kind == K::Delimiter)
            .map(|t| t.text(src))
            .collect();
        assert_eq!(d, vec!["DELIMITER //", "//", "DELIMITER ;"]);
        assert_eq!(delimiter_command_arg(&t[0], src), Some("//"));
        let semis = t.iter().filter(|t| t.kind == K::Semicolon).count();
        assert_eq!(semis, 2);
        // `$$` delimiter glued to END
        let src = "delimiter $$\nCREATE PROCEDURE p() BEGIN END$$\ndelimiter ;";
        let t = tokenize(src, &mysql());
        let d: Vec<_> = t
            .iter()
            .filter(|t| t.kind == K::Delimiter)
            .map(|t| t.text(src))
            .collect();
        assert_eq!(d, vec!["delimiter $$", "$$", "delimiter ;"]);
        assert!(
            t.iter()
                .any(|x| x.text(src) == "END" && x.kind == K::Keyword)
        );
        // Column called delimiter in the middle of a statement is not a command
        let src = "SELECT a,\ndelimiter FROM t";
        assert!(
            tokenize(src, &mysql())
                .iter()
                .all(|t| t.kind != K::Delimiter)
        );
        // Custom initial delimiter
        let src = "SELECT 1 GO SELECT go_x GO";
        let t = tokenize_with_delimiter(src, &pg(), "go");
        let d: Vec<_> = t
            .iter()
            .filter(|t| t.kind == K::Delimiter)
            .map(|t| t.text(src))
            .collect();
        assert_eq!(d, vec!["GO", "GO"]);
        assert!(t.iter().any(|t| t.text(src) == "go_x"));
    }

    #[test]
    fn batch_separator() {
        let src = "SELECT 1\nGO\nSELECT 'GO'\n go 2\nSELECT go FROM t";
        let t = tokenize(src, &tsql());
        let d: Vec<_> = t
            .iter()
            .filter(|t| t.kind == K::Delimiter)
            .map(|t| t.text(src))
            .collect();
        assert_eq!(d, vec!["GO", "go 2"]);
        assert!(
            tokenize("SELECT 1\nGO", &pg())
                .iter()
                .all(|t| t.kind != K::Delimiter)
        );
    }

    #[test]
    fn unicode() {
        let src = "SELECT naïve, 名前\u{3000}FROM tablé — x";
        let t = tokenize(src, &pg());
        check_cover(src, &t);
        let k = kinds(src, &pg());
        assert_eq!(k[1], (K::Identifier, "naïve".into()));
        assert_eq!(k[3], (K::Identifier, "名前".into()));
        assert_eq!(k[4].0, K::Keyword);
        assert_eq!(k[6], (K::Unknown, "—".into()));
    }

    #[test]
    fn coverage_on_edge_inputs() {
        for src in [
            "", "'", "\"", "`", "[", "$", "${", "$a", "/*", "--", "#", "@", ":", "?", "\r\n\r",
            "E'", "U&", "0x", "1e", ".", "é'", "$$$", "@@", "@'", ":a::b",
        ] {
            for syn in [pg(), mysql(), sqlite(), tsql()] {
                let t = tokenize(src, &syn);
                check_cover(src, &t);
            }
        }
    }
}
