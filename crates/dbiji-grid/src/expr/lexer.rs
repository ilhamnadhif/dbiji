//! Tokenizer for the expression language.

use super::ExprError;
use dbiji_core::trf;
use std::ops::Range;

/// A lexical token with its byte span in the source text.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Token {
    pub kind: Tok,
    pub span: Range<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tok {
    /// Integer literal.
    Int(i64),
    /// Floating point literal.
    Float(f64),
    /// `'single quoted'` string literal.
    Str(String),
    /// Bare identifier (column name, function name or keyword).
    Ident(String),
    /// `[bracketed]` or `"double quoted"` column name; never a keyword.
    QuotedIdent(String),
    LParen,
    RParen,
    Comma,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Concat,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Eof,
}

impl Tok {
    /// Human readable description used in error messages.
    pub fn describe(&self) -> String {
        match self {
            Tok::Int(i) => i.to_string(),
            Tok::Float(f) => f.to_string(),
            Tok::Str(s) => format!("'{s}'"),
            Tok::Ident(s) => s.clone(),
            Tok::QuotedIdent(s) => format!("[{s}]"),
            Tok::LParen => "(".into(),
            Tok::RParen => ")".into(),
            Tok::Comma => ",".into(),
            Tok::Plus => "+".into(),
            Tok::Minus => "-".into(),
            Tok::Star => "*".into(),
            Tok::Slash => "/".into(),
            Tok::Percent => "%".into(),
            Tok::Concat => "||".into(),
            Tok::Eq => "=".into(),
            Tok::NotEq => "<>".into(),
            Tok::Lt => "<".into(),
            Tok::LtEq => "<=".into(),
            Tok::Gt => ">".into(),
            Tok::GtEq => ">=".into(),
            Tok::Eof => dbiji_core::tr("end of expression").to_string(),
        }
    }
}

/// Split `src` into tokens. The last token is always [`Tok::Eof`].
pub(crate) fn tokenize(src: &str) -> Result<Vec<Token>, ExprError> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let simple = |kind: Tok, len: usize, out: &mut Vec<Token>| {
            out.push(Token { kind, span: start..start + len });
            len
        };
        match c {
            b'(' => i += simple(Tok::LParen, 1, &mut out),
            b')' => i += simple(Tok::RParen, 1, &mut out),
            b',' => i += simple(Tok::Comma, 1, &mut out),
            b'+' => i += simple(Tok::Plus, 1, &mut out),
            b'-' => i += simple(Tok::Minus, 1, &mut out),
            b'*' => i += simple(Tok::Star, 1, &mut out),
            b'/' => i += simple(Tok::Slash, 1, &mut out),
            b'%' => i += simple(Tok::Percent, 1, &mut out),
            b'|' => {
                if bytes.get(i + 1) == Some(&b'|') {
                    i += simple(Tok::Concat, 2, &mut out);
                } else {
                    return Err(ExprError::new(
                        dbiji_core::tr("Unexpected '|' (use || to concatenate)"),
                        start..start + 1,
                    ));
                }
            }
            b'=' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    i += simple(Tok::Eq, 2, &mut out);
                } else {
                    i += simple(Tok::Eq, 1, &mut out);
                }
            }
            b'!' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    i += simple(Tok::NotEq, 2, &mut out);
                } else {
                    return Err(ExprError::new(dbiji_core::tr("Unexpected '!' (use NOT or !=)"), start..start + 1));
                }
            }
            b'<' => match bytes.get(i + 1) {
                Some(b'=') => i += simple(Tok::LtEq, 2, &mut out),
                Some(b'>') => i += simple(Tok::NotEq, 2, &mut out),
                _ => i += simple(Tok::Lt, 1, &mut out),
            },
            b'>' => match bytes.get(i + 1) {
                Some(b'=') => i += simple(Tok::GtEq, 2, &mut out),
                _ => i += simple(Tok::Gt, 1, &mut out),
            },
            b'\'' => {
                let (s, end) = read_quoted(src, i, '\'', '\'')?;
                out.push(Token { kind: Tok::Str(s), span: start..end });
                i = end;
            }
            b'"' => {
                let (s, end) = read_quoted(src, i, '"', '"')?;
                out.push(Token { kind: Tok::QuotedIdent(s), span: start..end });
                i = end;
            }
            b'[' => {
                let Some(rel) = src[i + 1..].find(']') else {
                    return Err(ExprError::new(dbiji_core::tr("Unterminated [column name]"), start..src.len()));
                };
                let name = src[i + 1..i + 1 + rel].to_string();
                let end = i + 1 + rel + 1;
                if name.trim().is_empty() {
                    return Err(ExprError::new(dbiji_core::tr("Empty column name"), start..end));
                }
                out.push(Token { kind: Tok::QuotedIdent(name), span: start..end });
                i = end;
            }
            b'0'..=b'9' | b'.' => {
                let (tok, end) = read_number(src, i)?;
                out.push(Token { kind: tok, span: start..end });
                i = end;
            }
            _ => {
                let ch = src[i..].chars().next().unwrap_or('\0');
                if ch == '_' || ch.is_alphabetic() {
                    let mut end = i;
                    for (off, c) in src[i..].char_indices() {
                        if c == '_' || c.is_alphanumeric() || c == '$' {
                            end = i + off + c.len_utf8();
                        } else {
                            break;
                        }
                    }
                    out.push(Token { kind: Tok::Ident(src[i..end].to_string()), span: start..end });
                    i = end;
                } else {
                    return Err(ExprError::new(trf("Unexpected character '{}'", &[&ch]), start..start + ch.len_utf8()));
                }
            }
        }
    }
    out.push(Token { kind: Tok::Eof, span: src.len()..src.len() });
    Ok(out)
}

/// Read a quoted literal starting at `start` (which holds `open`); a doubled
/// `close` character is an escaped quote. Returns the content and the end offset.
fn read_quoted(src: &str, start: usize, open: char, close: char) -> Result<(String, usize), ExprError> {
    let mut s = String::new();
    let mut iter = src[start + open.len_utf8()..].char_indices().peekable();
    let base = start + open.len_utf8();
    while let Some((off, c)) = iter.next() {
        if c == close {
            if let Some(&(_, next)) = iter.peek()
                && next == close
            {
                s.push(close);
                iter.next();
                continue;
            }
            return Ok((s, base + off + close.len_utf8()));
        }
        s.push(c);
    }
    let msg = if open == '\'' {
        dbiji_core::tr("Unterminated string literal")
    } else {
        dbiji_core::tr("Unterminated quoted column name")
    };
    Err(ExprError::new(msg, start..src.len()))
}

fn read_number(src: &str, start: usize) -> Result<(Tok, usize), ExprError> {
    let bytes = src.as_bytes();
    let mut i = start;
    let mut is_float = false;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'.' {
        is_float = true;
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        let mut j = i + 1;
        if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
            j += 1;
        }
        if j < bytes.len() && bytes[j].is_ascii_digit() {
            is_float = true;
            i = j;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
        }
    }
    let text = &src[start..i];
    if text == "." {
        return Err(ExprError::new(dbiji_core::tr("Unexpected '.'"), start..start + 1));
    }
    if i < bytes.len() && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
        return Err(ExprError::new(trf("Invalid number '{}'", &[&&src[start..=i]]), start..i + 1));
    }
    if !is_float && let Ok(v) = text.parse::<i64>() {
        return Ok((Tok::Int(v), i));
    }
    match text.parse::<f64>() {
        Ok(f) => Ok((Tok::Float(f), i)),
        Err(_) => Err(ExprError::new(trf("Invalid number '{}'", &[&text]), start..i)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(s: &str) -> Vec<Tok> {
        tokenize(s).unwrap().into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn basic_tokens() {
        assert_eq!(
            kinds("a + 1.5 * [my col] || 'it''s' <> 3"),
            vec![
                Tok::Ident("a".into()),
                Tok::Plus,
                Tok::Float(1.5),
                Tok::Star,
                Tok::QuotedIdent("my col".into()),
                Tok::Concat,
                Tok::Str("it's".into()),
                Tok::NotEq,
                Tok::Int(3),
                Tok::Eof
            ]
        );
        assert_eq!(
            kinds("x<=1>=2!=3==4"),
            vec![
                Tok::Ident("x".into()),
                Tok::LtEq,
                Tok::Int(1),
                Tok::GtEq,
                Tok::Int(2),
                Tok::NotEq,
                Tok::Int(3),
                Tok::Eq,
                Tok::Int(4),
                Tok::Eof
            ]
        );
        assert_eq!(
            kinds("1e3 .5 \"q\"\"x\""),
            vec![Tok::Float(1000.0), Tok::Float(0.5), Tok::QuotedIdent("q\"x".into()), Tok::Eof]
        );
        assert_eq!(kinds("größe_1"), vec![Tok::Ident("größe_1".into()), Tok::Eof]);
    }

    #[test]
    fn errors_have_spans() {
        let e = tokenize("a + 'abc").unwrap_err();
        assert_eq!(e.span, 4..8);
        let e = tokenize("a # b").unwrap_err();
        assert_eq!(e.span, 2..3);
        assert!(tokenize("[unclosed").is_err());
        assert!(tokenize("12abc").is_err());
        assert!(tokenize("a | b").is_err());
    }
}
