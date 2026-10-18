//! Syntax highlighting classes for the SQL editor.

use crate::lexicon::{Lexicon, reserved};
use crate::token::{Token, TokenKind, delimiter_command_arg, is_word, tokenize};
use dbiji_core::SqlSyntax;
use serde::{Deserialize, Serialize};
use std::ops::Range;

/// Visual class of a source range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HighlightClass {
    /// Keyword.
    Keyword,
    /// Unquoted identifier.
    Identifier,
    /// Quoted identifier.
    QuotedIdentifier,
    /// String literal (including dollar-quoted bodies).
    String,
    /// Numeric literal.
    Number,
    /// Line or block comment.
    Comment,
    /// Operator.
    Operator,
    /// `,` `;` `.` `(` `)` `[` `]` and statement delimiters.
    Punctuation,
    /// `?`, `:name`, `$1`
    Parameter,
    /// `@var`, `@@var`, `${name}`
    Variable,
    /// Identifier followed by `(` or listed in [`SqlSyntax::functions`].
    Function,
    /// Type names after `::`, `CAST(x AS`, a column name in a definition, `RETURNS` ...
    DataType,
    /// `@set ...` lines and `DELIMITER` commands.
    ClientCommand,
    /// Anything else (stray characters).
    Plain,
}

/// Words after which `name (` is not a function call (`INSERT INTO t (a)`).
const NOT_CALL_AFTER: &[&str] = &[
    "INTO",
    "TABLE",
    "REFERENCES",
    "EXISTS",
    "VIEW",
    "INDEX",
    "TRIGGER",
    "UPDATE",
    "JOIN",
];
/// Functions whose `AS` introduces a type.
const CAST_FUNCTIONS: &[&str] = &["CAST", "TRY_CAST", "SAFE_CAST", "CONVERT"];
/// Words after which a listed type name is a type.
const TYPE_AFTER: &[&str] = &["TYPE", "RETURNS", "SETOF", "VARYING"];

/// Highlight ranges for every non-whitespace token of `src`, in order.
pub fn highlight(src: &str, syntax: &SqlSyntax) -> Vec<(Range<usize>, HighlightClass)> {
    let tokens = tokenize(src, syntax);
    highlight_tokens(src, &tokens, syntax)
}

pub(crate) fn highlight_tokens(
    src: &str,
    tokens: &[Token],
    syntax: &SqlSyntax,
) -> Vec<(Range<usize>, HighlightClass)> {
    use HighlightClass as H;
    let lex = Lexicon::new(syntax);
    let reserved = reserved();
    let sig: Vec<usize> = (0..tokens.len())
        .filter(|&i| !tokens[i].is_trivia())
        .collect();
    let mut out = Vec::with_capacity(sig.len() + 8);
    // Function names owning the currently open parentheses (for CAST(... AS type)).
    let mut parens: Vec<Option<usize>> = Vec::new();
    let mut prev_class: Option<H> = None;
    let mut last_sig: Option<usize> = None;
    let mut si = 0usize;
    for (i, t) in tokens.iter().enumerate() {
        if t.kind == TokenKind::Whitespace || t.kind == TokenKind::Newline {
            continue;
        }
        let class = match t.kind {
            TokenKind::LineComment | TokenKind::BlockComment => H::Comment,
            TokenKind::String => H::String,
            TokenKind::Number => H::Number,
            TokenKind::Operator => H::Operator,
            TokenKind::Comma
            | TokenKind::Semicolon
            | TokenKind::Dot
            | TokenKind::RParen
            | TokenKind::LBracket
            | TokenKind::RBracket => H::Punctuation,
            TokenKind::LParen => H::Punctuation,
            TokenKind::Parameter => H::Parameter,
            TokenKind::Variable => H::Variable,
            TokenKind::ClientCommand => H::ClientCommand,
            TokenKind::Delimiter => {
                if delimiter_command_arg(t, src).is_some() {
                    H::ClientCommand
                } else {
                    H::Punctuation
                }
            }
            TokenKind::Unknown | TokenKind::Whitespace | TokenKind::Newline => H::Plain,
            TokenKind::QuotedIdentifier => H::QuotedIdentifier,
            TokenKind::Keyword | TokenKind::Identifier => {
                // Position among significant tokens.
                while si < sig.len() && sig[si] < i {
                    si += 1;
                }
                let prev = if si > 0 {
                    sig.get(si - 1).map(|&j| &tokens[j])
                } else {
                    None
                };
                let prev2 = if si > 1 {
                    sig.get(si - 2).map(|&j| &tokens[j])
                } else {
                    None
                };
                let next = sig.get(si + 1).map(|&j| &tokens[j]);
                classify_word(
                    src,
                    t,
                    prev,
                    prev2,
                    next,
                    prev_class,
                    parens.last().copied().flatten().map(|j| &tokens[j]),
                    &lex,
                    &reserved,
                )
            }
        };
        match t.kind {
            TokenKind::LParen => {
                let owner = last_sig.filter(|&j| tokens[j].kind.is_word());
                parens.push(owner);
            }
            TokenKind::RParen => {
                parens.pop();
            }
            TokenKind::Semicolon | TokenKind::Delimiter => parens.clear(),
            _ => {}
        }
        if !t.kind.is_comment() {
            prev_class = Some(class);
            last_sig = Some(i);
        }
        out.push((t.start..t.end, class));
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn classify_word(
    src: &str,
    t: &Token,
    prev: Option<&Token>,
    prev2: Option<&Token>,
    next: Option<&Token>,
    prev_class: Option<HighlightClass>,
    paren_owner: Option<&Token>,
    lex: &Lexicon,
    reserved: &crate::lexicon::WordSet,
) -> HighlightClass {
    use HighlightClass as H;
    let text = t.text(src);
    let listed_type = lex.data_types.contains(text);
    let prev_is = |w: &str| prev.is_some_and(|p| is_word(p, src, w));
    let in_cast = paren_owner.is_some_and(|o| CAST_FUNCTIONS.iter().any(|f| is_word(o, src, f)));
    // Type positions.
    if prev.is_some_and(|p| p.kind == TokenKind::Operator && p.text(src) == "::") {
        return H::DataType;
    }
    if in_cast && prev_is("AS") {
        return H::DataType;
    }
    if listed_type {
        if prev_class == Some(H::DataType) {
            return H::DataType; // DOUBLE PRECISION, TIMESTAMP WITH TIME ZONE
        }
        if let Some(p) = prev {
            match p.kind {
                TokenKind::Identifier | TokenKind::QuotedIdentifier | TokenKind::Variable => {
                    return H::DataType;
                }
                TokenKind::Keyword => {
                    if TYPE_AFTER.iter().any(|w| is_word(p, src, w)) {
                        return H::DataType;
                    }
                    // A non-reserved keyword used as a column name: `(name TEXT, ...`
                    let at_def = prev2.is_some_and(|q| {
                        matches!(q.kind, TokenKind::LParen | TokenKind::Comma)
                            || is_word(q, src, "COLUMN")
                    });
                    if at_def && !reserved.contains(p.text(src)) {
                        return H::DataType;
                    }
                }
                _ => {}
            }
        }
    }
    let call = next.is_some_and(|n| n.kind == TokenKind::LParen);
    let not_call = prev.is_some_and(|p| NOT_CALL_AFTER.iter().any(|w| is_word(p, src, w)));
    match t.kind {
        TokenKind::Keyword => {
            if call && lex.functions.contains(text) && !not_call {
                H::Function
            } else {
                H::Keyword
            }
        }
        _ => {
            if (call && !not_call) || lex.functions.contains(text) {
                H::Function
            } else {
                H::Identifier
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use HighlightClass as H;

    fn hl(src: &str, syn: &SqlSyntax) -> Vec<(String, H)> {
        highlight(src, syn)
            .into_iter()
            .map(|(r, c)| (src[r].to_string(), c))
            .collect()
    }

    fn class_of(src: &str, syn: &SqlSyntax, word: &str) -> H {
        hl(src, syn)
            .into_iter()
            .find(|(t, _)| t == word)
            .map(|(_, c)| c)
            .unwrap_or(H::Plain)
    }

    #[test]
    fn basic_classes() {
        let src = "SELECT u.id, 'x', 42, ? FROM \"Users\" u -- c\nWHERE a = :p";
        let h = hl(src, &mysql());
        assert_eq!(h[0], ("SELECT".into(), H::Keyword));
        assert_eq!(h[1], ("u".into(), H::Identifier));
        assert_eq!(h[2], (".".into(), H::Punctuation));
        assert!(h.contains(&("'x'".into(), H::String)));
        assert!(h.contains(&("42".into(), H::Number)));
        assert!(h.contains(&("?".into(), H::Parameter)));
        assert!(h.contains(&("\"Users\"".into(), H::String))); // MySQL: "..." is a string
        assert!(h.contains(&("-- c".into(), H::Comment)));
        assert!(h.contains(&("=".into(), H::Operator)));
        assert!(h.contains(&(":p".into(), H::Parameter)));
        let h = hl("SELECT \"Users\".x, @v, ${t}", &pg());
        assert!(h.contains(&("\"Users\"".into(), H::QuotedIdentifier)));
        assert!(h.contains(&("${t}".into(), H::Variable)));
    }

    #[test]
    fn functions() {
        let src = "SELECT count(*), my_fn (1), coalesce, now FROM t";
        assert_eq!(class_of(src, &pg(), "count"), H::Function);
        assert_eq!(class_of(src, &pg(), "my_fn"), H::Function);
        assert_eq!(class_of(src, &pg(), "coalesce"), H::Function);
        assert_eq!(class_of(src, &pg(), "now"), H::Function);
        assert_eq!(class_of(src, &pg(), "t"), H::Identifier);
        // Not calls
        assert_eq!(
            class_of("INSERT INTO t (a) VALUES (1)", &pg(), "t"),
            H::Identifier
        );
        assert_eq!(
            class_of("INSERT INTO t (a) VALUES (1)", &pg(), "VALUES"),
            H::Keyword
        );
        assert_eq!(class_of("SELECT 1 WHERE a IN (1)", &pg(), "IN"), H::Keyword);
        assert_eq!(
            class_of("CREATE OR REPLACE VIEW v AS SELECT 1", &pg(), "REPLACE"),
            H::Keyword
        );
        assert_eq!(
            class_of("SELECT replace(a, 'x', 'y')", &mysql(), "replace"),
            H::Function
        );
        assert_eq!(class_of("SELECT IF(a, 1, 2)", &mysql(), "IF"), H::Function);
    }

    #[test]
    fn data_types() {
        let src = "CREATE TABLE t (id INTEGER, name TEXT, price NUMERIC(10, 2), amount DOUBLE PRECISION, x VARCHAR(5))";
        assert_eq!(class_of(src, &pg(), "INTEGER"), H::DataType);
        assert_eq!(class_of(src, &pg(), "TEXT"), H::DataType);
        assert_eq!(class_of(src, &pg(), "NUMERIC"), H::DataType);
        assert_eq!(class_of(src, &pg(), "DOUBLE"), H::DataType);
        assert_eq!(class_of(src, &pg(), "PRECISION"), H::DataType);
        assert_eq!(class_of(src, &pg(), "VARCHAR"), H::DataType);
        assert_eq!(
            class_of(
                "SELECT a::my_type, CAST(b AS int8) FROM t",
                &pg(),
                "my_type"
            ),
            H::DataType
        );
        assert_eq!(
            class_of("SELECT CAST(b AS int8) FROM t", &pg(), "int8"),
            H::DataType
        );
        assert_eq!(class_of("SELECT text FROM t", &pg(), "text"), H::Identifier);
        assert_eq!(class_of("DECLARE @x INT", &tsql(), "INT"), H::DataType);
        assert_eq!(
            class_of("ALTER TABLE t ALTER COLUMN c TYPE BIGINT", &pg(), "BIGINT"),
            H::DataType
        );
    }

    #[test]
    fn commands_and_delimiters() {
        let src = "@set x = 1\nDELIMITER //\nSELECT 1//\nDELIMITER ;";
        let h = hl(src, &mysql());
        assert_eq!(h[0], ("@set x = 1".into(), H::ClientCommand));
        assert_eq!(h[1], ("DELIMITER //".into(), H::ClientCommand));
        assert!(h.contains(&("//".into(), H::Punctuation)));
    }

    #[test]
    fn covers_all_non_whitespace() {
        let mut rng = Rng::new(17);
        for _ in 0..300 {
            let src = random_sql(&mut rng, 30);
            for syn in all_syntaxes() {
                let h = highlight(&src, &syn);
                let mut last = 0;
                for (r, _) in &h {
                    assert!(
                        r.start >= last
                            && r.start < r.end
                            && src.is_char_boundary(r.start)
                            && src.is_char_boundary(r.end)
                    );
                    assert!(
                        src[last..r.start].chars().all(char::is_whitespace),
                        "uncovered text in {src:?}"
                    );
                    last = r.end;
                }
                assert!(src[last..].chars().all(char::is_whitespace));
            }
        }
    }
}
