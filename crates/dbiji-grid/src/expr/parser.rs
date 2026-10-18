//! Recursive descent parser producing an unbound AST.
//!
//! Grammar (lowest to highest precedence):
//!
//! ```text
//! expr    := or
//! or      := and (OR and)*
//! and     := not (AND not)*
//! not     := NOT not | cmp
//! cmp     := concat ( (= | == | <> | != | < | <= | > | >=) concat
//!                   | IS [NOT] NULL
//!                   | [NOT] (LIKE | ILIKE) concat
//!                   | [NOT] IN '(' expr (',' expr)* ')'
//!                   | [NOT] BETWEEN concat AND concat )?
//! concat  := add ('||' add)*
//! add     := mul (('+' | '-') mul)*
//! mul     := unary (('*' | '/' | '%') unary)*
//! unary   := ('-' | '+') unary | primary
//! primary := number | string | TRUE | FALSE | NULL | name | [name] | "name"
//!          | function '(' [expr (',' expr)*] ')' | '(' expr ')'
//! ```

use super::ExprError;
use super::lexer::{Tok, Token, tokenize};
use dbiji_core::{Value, tr, trf};
use std::ops::Range;

/// Binary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Rem,
    /// `||`
    Concat,
    /// `=` / `==`
    Eq,
    /// `<>` / `!=`
    NotEq,
    /// `<`
    Lt,
    /// `<=`
    LtEq,
    /// `>`
    Gt,
    /// `>=`
    GtEq,
    /// `AND`
    And,
    /// `OR`
    Or,
}

/// Unary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// Arithmetic negation.
    Neg,
    /// Logical `NOT`.
    Not,
}

/// Unbound syntax tree; column references are still names.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Ast {
    Literal(Value),
    Column { name: String, span: Range<usize> },
    Unary(UnaryOp, Box<Ast>),
    Binary(BinaryOp, Box<Ast>, Box<Ast>),
    IsNull { expr: Box<Ast>, negated: bool },
    Like { expr: Box<Ast>, pattern: Box<Ast>, negated: bool, case_insensitive: bool },
    In { expr: Box<Ast>, list: Vec<Ast>, negated: bool },
    Between { expr: Box<Ast>, low: Box<Ast>, high: Box<Ast>, negated: bool },
    Call { name: String, args: Vec<Ast>, span: Range<usize> },
}

const KEYWORDS: &[&str] = &["AND", "OR", "NOT", "IS", "NULL", "TRUE", "FALSE", "LIKE", "ILIKE", "IN", "BETWEEN"];

fn is_keyword(s: &str) -> bool {
    KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(s))
}

pub(crate) fn parse(src: &str) -> Result<Ast, ExprError> {
    let tokens = tokenize(src)?;
    let mut p = Parser { tokens, pos: 0 };
    if matches!(p.peek().kind, Tok::Eof) {
        return Err(ExprError::new(tr("Expression is empty"), 0..0));
    }
    let ast = p.expr(0)?;
    let t = p.peek();
    if !matches!(t.kind, Tok::Eof) {
        return Err(ExprError::new(trf("Unexpected '{}' after the end of the expression", &[&t.kind.describe()]), t.span.clone()));
    }
    Ok(ast)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

const MAX_DEPTH: usize = 200;

impl Parser {
    fn peek(&self) -> &Token {
        // The token list always ends with Eof and `pos` never passes it.
        &self.tokens[self.pos.min(self.tokens.len() - 1)]
    }

    fn peek_at(&self, offset: usize) -> &Token {
        &self.tokens[(self.pos + offset).min(self.tokens.len() - 1)]
    }

    fn next(&mut self) -> Token {
        let t = self.peek().clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn is_kw(&self, kw: &str) -> bool {
        matches!(&self.peek().kind, Tok::Ident(s) if s.eq_ignore_ascii_case(kw))
    }

    fn is_kw_at(&self, offset: usize, kw: &str) -> bool {
        matches!(&self.peek_at(offset).kind, Tok::Ident(s) if s.eq_ignore_ascii_case(kw))
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.next();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: Tok, what: &str) -> Result<Token, ExprError> {
        let t = self.peek().clone();
        if t.kind == kind {
            self.next();
            Ok(t)
        } else {
            Err(ExprError::new(trf("Expected {} but found '{}'", &[&what, &t.kind.describe()]), t.span))
        }
    }

    fn check_depth(&self, depth: usize) -> Result<(), ExprError> {
        if depth > MAX_DEPTH { Err(ExprError::new(tr("Expression is nested too deeply"), self.peek().span.clone())) } else { Ok(()) }
    }

    fn expr(&mut self, depth: usize) -> Result<Ast, ExprError> {
        self.check_depth(depth)?;
        self.or(depth + 1)
    }

    fn or(&mut self, depth: usize) -> Result<Ast, ExprError> {
        let mut left = self.and(depth)?;
        while self.eat_kw("OR") {
            let right = self.and(depth)?;
            left = Ast::Binary(BinaryOp::Or, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and(&mut self, depth: usize) -> Result<Ast, ExprError> {
        let mut left = self.not(depth)?;
        while self.eat_kw("AND") {
            let right = self.not(depth)?;
            left = Ast::Binary(BinaryOp::And, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn not(&mut self, depth: usize) -> Result<Ast, ExprError> {
        self.check_depth(depth)?;
        if self.eat_kw("NOT") {
            let inner = self.not(depth + 1)?;
            return Ok(Ast::Unary(UnaryOp::Not, Box::new(inner)));
        }
        self.cmp(depth)
    }

    fn cmp(&mut self, depth: usize) -> Result<Ast, ExprError> {
        let left = self.concat(depth)?;
        let op = match self.peek().kind {
            Tok::Eq => Some(BinaryOp::Eq),
            Tok::NotEq => Some(BinaryOp::NotEq),
            Tok::Lt => Some(BinaryOp::Lt),
            Tok::LtEq => Some(BinaryOp::LtEq),
            Tok::Gt => Some(BinaryOp::Gt),
            Tok::GtEq => Some(BinaryOp::GtEq),
            _ => None,
        };
        if let Some(op) = op {
            self.next();
            let right = self.concat(depth)?;
            return Ok(Ast::Binary(op, Box::new(left), Box::new(right)));
        }
        if self.is_kw("IS") {
            self.next();
            let negated = self.eat_kw("NOT");
            if !self.eat_kw("NULL") {
                let t = self.peek().clone();
                return Err(ExprError::new(trf("Expected NULL after IS but found '{}'", &[&t.kind.describe()]), t.span));
            }
            return Ok(Ast::IsNull { expr: Box::new(left), negated });
        }
        let negated = if self.is_kw("NOT") && (self.is_kw_at(1, "LIKE") || self.is_kw_at(1, "ILIKE") || self.is_kw_at(1, "IN") || self.is_kw_at(1, "BETWEEN")) {
            self.next();
            true
        } else {
            false
        };
        if self.is_kw("LIKE") || self.is_kw("ILIKE") {
            let case_insensitive = self.is_kw("ILIKE");
            self.next();
            let pattern = self.concat(depth)?;
            return Ok(Ast::Like { expr: Box::new(left), pattern: Box::new(pattern), negated, case_insensitive });
        }
        if self.eat_kw("IN") {
            self.expect(Tok::LParen, "'('")?;
            let mut list = vec![self.expr(depth + 1)?];
            while matches!(self.peek().kind, Tok::Comma) {
                self.next();
                list.push(self.expr(depth + 1)?);
            }
            self.expect(Tok::RParen, "')'")?;
            return Ok(Ast::In { expr: Box::new(left), list, negated });
        }
        if self.eat_kw("BETWEEN") {
            let low = self.concat(depth)?;
            if !self.eat_kw("AND") {
                let t = self.peek().clone();
                return Err(ExprError::new(trf("Expected AND in BETWEEN but found '{}'", &[&t.kind.describe()]), t.span));
            }
            let high = self.concat(depth)?;
            return Ok(Ast::Between { expr: Box::new(left), low: Box::new(low), high: Box::new(high), negated });
        }
        if negated {
            // Unreachable: `negated` is only set when one of the keywords follows.
            let t = self.peek().clone();
            return Err(ExprError::new(tr("Unexpected NOT"), t.span));
        }
        Ok(left)
    }

    fn concat(&mut self, depth: usize) -> Result<Ast, ExprError> {
        let mut left = self.add(depth)?;
        while matches!(self.peek().kind, Tok::Concat) {
            self.next();
            let right = self.add(depth)?;
            left = Ast::Binary(BinaryOp::Concat, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn add(&mut self, depth: usize) -> Result<Ast, ExprError> {
        let mut left = self.mul(depth)?;
        loop {
            let op = match self.peek().kind {
                Tok::Plus => BinaryOp::Add,
                Tok::Minus => BinaryOp::Sub,
                _ => break,
            };
            self.next();
            let right = self.mul(depth)?;
            left = Ast::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn mul(&mut self, depth: usize) -> Result<Ast, ExprError> {
        let mut left = self.unary(depth)?;
        loop {
            let op = match self.peek().kind {
                Tok::Star => BinaryOp::Mul,
                Tok::Slash => BinaryOp::Div,
                Tok::Percent => BinaryOp::Rem,
                _ => break,
            };
            self.next();
            let right = self.unary(depth)?;
            left = Ast::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn unary(&mut self, depth: usize) -> Result<Ast, ExprError> {
        self.check_depth(depth)?;
        match self.peek().kind {
            Tok::Minus => {
                self.next();
                let inner = self.unary(depth + 1)?;
                // Fold negative numeric literals so `-9223372036854775808` style values work.
                Ok(match inner {
                    Ast::Literal(Value::Int(i)) => Ast::Literal(match i.checked_neg() {
                        Some(n) => Value::Int(n),
                        None => Value::Float(-(i as f64)),
                    }),
                    Ast::Literal(Value::Float(f)) => Ast::Literal(Value::Float(-f)),
                    other => Ast::Unary(UnaryOp::Neg, Box::new(other)),
                })
            }
            Tok::Plus => {
                self.next();
                self.unary(depth + 1)
            }
            _ => self.primary(depth),
        }
    }

    fn primary(&mut self, depth: usize) -> Result<Ast, ExprError> {
        let t = self.next();
        match t.kind {
            Tok::Int(i) => Ok(Ast::Literal(Value::Int(i))),
            Tok::Float(f) => Ok(Ast::Literal(Value::Float(f))),
            Tok::Str(s) => Ok(Ast::Literal(Value::text(s))),
            Tok::QuotedIdent(name) => Ok(Ast::Column { name, span: t.span }),
            Tok::LParen => {
                let inner = self.expr(depth + 1)?;
                self.expect(Tok::RParen, "')'")?;
                Ok(inner)
            }
            Tok::Ident(word) => {
                if word.eq_ignore_ascii_case("NULL") {
                    return Ok(Ast::Literal(Value::Null));
                }
                if word.eq_ignore_ascii_case("TRUE") {
                    return Ok(Ast::Literal(Value::Bool(true)));
                }
                if word.eq_ignore_ascii_case("FALSE") {
                    return Ok(Ast::Literal(Value::Bool(false)));
                }
                if matches!(self.peek().kind, Tok::LParen) {
                    self.next();
                    let mut args = Vec::new();
                    if !matches!(self.peek().kind, Tok::RParen) {
                        args.push(self.expr(depth + 1)?);
                        while matches!(self.peek().kind, Tok::Comma) {
                            self.next();
                            args.push(self.expr(depth + 1)?);
                        }
                    }
                    let close = self.expect(Tok::RParen, "')'")?;
                    return Ok(Ast::Call { name: word, args, span: t.span.start..close.span.end });
                }
                if is_keyword(&word) {
                    return Err(ExprError::new(trf("'{}' is a keyword; write [{}] to refer to a column with that name", &[&word, &word]), t.span));
                }
                Ok(Ast::Column { name: word, span: t.span })
            }
            Tok::Eof => Err(ExprError::new(tr("Unexpected end of expression"), t.span)),
            other => Err(ExprError::new(trf("Unexpected '{}'", &[&other.describe()]), t.span)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(n: &str) -> Ast {
        Ast::Column { name: n.into(), span: 0..0 }
    }

    /// Structural comparison ignoring spans.
    fn strip(a: Ast) -> Ast {
        match a {
            Ast::Column { name, .. } => col(&name),
            Ast::Unary(op, e) => Ast::Unary(op, Box::new(strip(*e))),
            Ast::Binary(op, l, r) => Ast::Binary(op, Box::new(strip(*l)), Box::new(strip(*r))),
            Ast::IsNull { expr, negated } => Ast::IsNull { expr: Box::new(strip(*expr)), negated },
            Ast::Like { expr, pattern, negated, case_insensitive } => Ast::Like { expr: Box::new(strip(*expr)), pattern: Box::new(strip(*pattern)), negated, case_insensitive },
            Ast::In { expr, list, negated } => Ast::In { expr: Box::new(strip(*expr)), list: list.into_iter().map(strip).collect(), negated },
            Ast::Between { expr, low, high, negated } => Ast::Between { expr: Box::new(strip(*expr)), low: Box::new(strip(*low)), high: Box::new(strip(*high)), negated },
            Ast::Call { name, args, .. } => Ast::Call { name, args: args.into_iter().map(strip).collect(), span: 0..0 },
            other => other,
        }
    }

    fn p(s: &str) -> Ast {
        strip(parse(s).unwrap())
    }

    fn bin(op: BinaryOp, l: Ast, r: Ast) -> Ast {
        Ast::Binary(op, Box::new(l), Box::new(r))
    }

    fn int(i: i64) -> Ast {
        Ast::Literal(Value::Int(i))
    }

    #[test]
    fn precedence() {
        assert_eq!(p("1 + 2 * 3"), bin(BinaryOp::Add, int(1), bin(BinaryOp::Mul, int(2), int(3))));
        assert_eq!(p("(1 + 2) * 3"), bin(BinaryOp::Mul, bin(BinaryOp::Add, int(1), int(2)), int(3)));
        assert_eq!(
            p("a = 1 or b = 2 and c = 3"),
            bin(BinaryOp::Or, bin(BinaryOp::Eq, col("a"), int(1)), bin(BinaryOp::And, bin(BinaryOp::Eq, col("b"), int(2)), bin(BinaryOp::Eq, col("c"), int(3))))
        );
        assert_eq!(p("a || b + 1"), bin(BinaryOp::Concat, col("a"), bin(BinaryOp::Add, col("b"), int(1))));
        assert_eq!(p("not a = 1"), Ast::Unary(UnaryOp::Not, Box::new(bin(BinaryOp::Eq, col("a"), int(1)))));
        assert_eq!(p("-5"), int(-5));
        assert_eq!(p("- -5"), int(5));
        assert_eq!(p("-a"), Ast::Unary(UnaryOp::Neg, Box::new(col("a"))));
    }

    #[test]
    fn predicates() {
        assert_eq!(p("a is not null"), Ast::IsNull { expr: Box::new(col("a")), negated: true });
        assert_eq!(p("name not ilike 'a%'"), Ast::Like { expr: Box::new(col("name")), pattern: Box::new(Ast::Literal(Value::text("a%"))), negated: true, case_insensitive: true });
        assert_eq!(p("x in (1, 2)"), Ast::In { expr: Box::new(col("x")), list: vec![int(1), int(2)], negated: false });
        assert_eq!(
            p("x not between 1 and 5 and y"),
            bin(BinaryOp::And, Ast::Between { expr: Box::new(col("x")), low: Box::new(int(1)), high: Box::new(int(5)), negated: true }, col("y"))
        );
        assert_eq!(p("coalesce([first name], 'x')"), Ast::Call { name: "coalesce".into(), args: vec![col("first name"), Ast::Literal(Value::text("x"))], span: 0..0 });
        assert_eq!(p("now()"), Ast::Call { name: "now".into(), args: vec![], span: 0..0 });
        assert_eq!(p("[and] = TRUE"), bin(BinaryOp::Eq, col("and"), Ast::Literal(Value::Bool(true))));
    }

    #[test]
    fn errors() {
        let e = parse("1 +").unwrap_err();
        assert!(e.message.contains("end"), "{}", e.message);
        let e = parse("a = = 1").unwrap_err();
        assert_eq!(e.span, 4..5);
        let e = parse("f(1, 2").unwrap_err();
        assert!(e.message.contains("')'"), "{}", e.message);
        let e = parse("a b").unwrap_err();
        assert_eq!(e.span, 2..3);
        assert!(parse("").is_err());
        assert!(parse("x is 5").is_err());
        assert!(parse("x between 1").is_err());
        assert!(parse("and = 1").is_err());
        let deep = "(".repeat(500) + "1" + &")".repeat(500);
        assert!(parse(&deep).is_err());
    }
}
