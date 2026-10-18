//! A small, safe expression language evaluated per row.
//!
//! Used for virtual (computed) columns and client side filters. The language
//! is SQL flavoured:
//!
//! * column references by name (`price`), `[any name]` or `"any name"`;
//! * literals: integers, decimals (`1.5`, `1e3`), `'text'` (`''` escapes a
//!   quote), `TRUE`, `FALSE`, `NULL`;
//! * arithmetic `+ - * / %`, concatenation `||`;
//! * comparisons `= == <> != < <= > >=`, `IS [NOT] NULL`, `[NOT] LIKE`,
//!   `[NOT] ILIKE`, `[NOT] IN (...)`, `[NOT] BETWEEN a AND b`;
//! * logic `AND`, `OR`, `NOT` with SQL three-valued semantics;
//! * functions `upper lower length substr trim ltrim rtrim replace left right
//!   round abs coalesce concat if nullif to_number to_text`.
//!
//! NULL propagates through operators like in SQL. Evaluation never panics;
//! runtime problems (division by zero, non-numeric operands) are reported as
//! [`EvalError`]s.
//!
//! ```
//! use dbiji_grid::expr::Expr;
//! use dbiji_core::Value;
//! let e = Expr::compile("upper(name) || ' #' || to_text(id * 2)", &["id", "name"]).unwrap();
//! let row = [Value::Int(21), Value::text("ada")];
//! assert_eq!(e.eval(&row[..]).unwrap(), Value::text("ADA #42"));
//! ```

mod eval;
mod lexer;
mod parser;

pub(crate) use eval::round_decimal_text;
pub use eval::{RowValues, compare_values, is_truthy, like_match};
pub use parser::{BinaryOp, UnaryOp};

use dbiji_core::{Value, tr, trf};
use parser::Ast;
use std::fmt;
use std::ops::Range;
use std::sync::Arc;

/// Error produced while parsing or binding an expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExprError {
    /// Translated, human readable message.
    pub message: String,
    /// Byte range of the offending part of the source text.
    pub span: Range<usize>,
}

impl ExprError {
    pub(crate) fn new(message: impl Into<String>, span: Range<usize>) -> Self {
        ExprError { message: message.into(), span }
    }

    /// Message followed by the 1-based character position, e.g.
    /// `Unknown column 'nmae' (did you mean 'name'?) at position 1`.
    pub fn display_with_position(&self, source: &str) -> String {
        let pos = source.get(..self.span.start.min(source.len())).map_or(0, |s| s.chars().count()) + 1;
        trf("{} at position {}", &[&self.message, &pos])
    }
}

impl fmt::Display for ExprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ExprError {}

/// Error produced while evaluating an expression for one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalError(pub String);

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EvalError {}

/// Built-in functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Func {
    /// `upper(text)`
    Upper,
    /// `lower(text)`
    Lower,
    /// `length(text)` in characters (bytes for binary values).
    Length,
    /// `substr(text, start [, length])`, 1-based.
    Substr,
    /// `trim(text)`
    Trim,
    /// `ltrim(text)`
    Ltrim,
    /// `rtrim(text)`
    Rtrim,
    /// `replace(text, from, to)`
    Replace,
    /// `left(text, n)`
    Left,
    /// `right(text, n)`
    Right,
    /// `round(number [, digits])`, half away from zero.
    Round,
    /// `abs(number)`
    Abs,
    /// `coalesce(a, b, ...)`: first non-NULL argument.
    Coalesce,
    /// `concat(a, b, ...)`: concatenation ignoring NULLs.
    Concat,
    /// `if(condition, then, else)`
    If,
    /// `nullif(a, b)`: NULL when `a = b`.
    NullIf,
    /// `to_number(text)`
    ToNumber,
    /// `to_text(value)`
    ToText,
}

impl Func {
    /// All functions with their canonical (lower case) name.
    pub const ALL: [(Func, &'static str); 18] = [
        (Func::Upper, "upper"),
        (Func::Lower, "lower"),
        (Func::Length, "length"),
        (Func::Substr, "substr"),
        (Func::Trim, "trim"),
        (Func::Ltrim, "ltrim"),
        (Func::Rtrim, "rtrim"),
        (Func::Replace, "replace"),
        (Func::Left, "left"),
        (Func::Right, "right"),
        (Func::Round, "round"),
        (Func::Abs, "abs"),
        (Func::Coalesce, "coalesce"),
        (Func::Concat, "concat"),
        (Func::If, "if"),
        (Func::NullIf, "nullif"),
        (Func::ToNumber, "to_number"),
        (Func::ToText, "to_text"),
    ];

    /// Look up a function by name (case-insensitive). `substring`, `len`,
    /// `iif`, `ifnull` and `nvl` are accepted as aliases.
    pub fn lookup(name: &str) -> Option<Func> {
        let lower = name.to_ascii_lowercase();
        let alias = match lower.as_str() {
            "substring" => "substr",
            "len" | "char_length" => "length",
            "iif" => "if",
            "ifnull" | "nvl" => "coalesce",
            "str" | "to_char" => "to_text",
            other => other,
        };
        Func::ALL.iter().find(|(_, n)| *n == alias).map(|(f, _)| *f)
    }

    /// Canonical name.
    pub fn name(self) -> &'static str {
        Func::ALL.iter().find(|(f, _)| *f == self).map_or("?", |(_, n)| n)
    }

    /// Allowed argument count range (inclusive).
    pub fn arity(self) -> (usize, usize) {
        match self {
            Func::Upper | Func::Lower | Func::Length | Func::Trim | Func::Ltrim | Func::Rtrim | Func::Abs => (1, 1),
            Func::ToNumber | Func::ToText => (1, 1),
            Func::Substr => (2, 3),
            Func::Replace => (3, 3),
            Func::Left | Func::Right | Func::NullIf => (2, 2),
            Func::Round => (1, 2),
            Func::Coalesce | Func::Concat => (1, usize::MAX),
            Func::If => (3, 3),
        }
    }
}

/// Bound expression tree: column references are resolved to indices.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    /// Constant value.
    Literal(Value),
    /// Value of the column with this index.
    Column(usize),
    /// Unary operator.
    Unary(UnaryOp, Box<Node>),
    /// Binary operator.
    Binary(BinaryOp, Box<Node>, Box<Node>),
    /// `expr IS [NOT] NULL`.
    IsNull {
        /// Tested expression.
        expr: Box<Node>,
        /// `IS NOT NULL`.
        negated: bool,
    },
    /// `expr [NOT] LIKE|ILIKE pattern`.
    Like {
        /// Tested expression.
        expr: Box<Node>,
        /// Pattern (`%` and `_` wildcards, `\` escapes).
        pattern: Box<Node>,
        /// `NOT LIKE`.
        negated: bool,
        /// `ILIKE`.
        case_insensitive: bool,
    },
    /// `expr [NOT] IN (list)`.
    In {
        /// Tested expression.
        expr: Box<Node>,
        /// Candidate values.
        list: Vec<Node>,
        /// `NOT IN`.
        negated: bool,
    },
    /// `expr [NOT] BETWEEN low AND high`.
    Between {
        /// Tested expression.
        expr: Box<Node>,
        /// Lower bound (inclusive).
        low: Box<Node>,
        /// Upper bound (inclusive).
        high: Box<Node>,
        /// `NOT BETWEEN`.
        negated: bool,
    },
    /// Function call.
    Call(Func, Vec<Node>),
}

impl Node {
    /// Indices of all referenced columns (sorted, unique).
    pub fn referenced_columns(&self) -> Vec<usize> {
        let mut out = Vec::new();
        self.collect_columns(&mut out);
        out.sort_unstable();
        out.dedup();
        out
    }

    fn collect_columns(&self, out: &mut Vec<usize>) {
        match self {
            Node::Literal(_) => {}
            Node::Column(c) => out.push(*c),
            Node::Unary(_, e) => e.collect_columns(out),
            Node::Binary(_, l, r) => {
                l.collect_columns(out);
                r.collect_columns(out);
            }
            Node::IsNull { expr, .. } => expr.collect_columns(out),
            Node::Like { expr, pattern, .. } => {
                expr.collect_columns(out);
                pattern.collect_columns(out);
            }
            Node::In { expr, list, .. } => {
                expr.collect_columns(out);
                for n in list {
                    n.collect_columns(out);
                }
            }
            Node::Between { expr, low, high, .. } => {
                expr.collect_columns(out);
                low.collect_columns(out);
                high.collect_columns(out);
            }
            Node::Call(_, args) => {
                for a in args {
                    a.collect_columns(out);
                }
            }
        }
    }
}

/// A compiled expression, cheap to clone and safe to share between threads.
#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    root: Arc<Node>,
    source: Arc<str>,
}

impl Expr {
    /// Parse `source` and resolve column names against `columns`.
    ///
    /// Names match exactly first, then case-insensitively; an ambiguous
    /// case-insensitive match is an error.
    pub fn compile<S: AsRef<str>>(source: &str, columns: &[S]) -> Result<Expr, ExprError> {
        let ast = parser::parse(source)?;
        let root = bind(ast, columns)?;
        Ok(Expr { root: Arc::new(root), source: source.into() })
    }

    /// Build an expression from an already bound tree (e.g. generated filters).
    pub fn from_node(node: Node, source: impl Into<String>) -> Expr {
        Expr { root: Arc::new(node), source: source.into().into() }
    }

    /// The source text.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The bound syntax tree.
    pub fn node(&self) -> &Node {
        &self.root
    }

    /// Evaluate for one row.
    pub fn eval<R: RowValues + ?Sized>(&self, row: &R) -> Result<Value, EvalError> {
        eval::eval(&self.root, row)
    }

    /// Evaluate as a filter predicate: only `TRUE` keeps the row; `NULL`,
    /// `FALSE` and evaluation errors reject it.
    pub fn matches<R: RowValues + ?Sized>(&self, row: &R) -> bool {
        self.eval(row).map(|v| is_truthy(&v)).unwrap_or(false)
    }

    /// Indices of the columns the expression reads.
    pub fn referenced_columns(&self) -> Vec<usize> {
        self.root.referenced_columns()
    }
}

fn resolve_column<S: AsRef<str>>(name: &str, columns: &[S], span: &Range<usize>) -> Result<usize, ExprError> {
    if let Some(i) = columns.iter().position(|c| c.as_ref() == name) {
        return Ok(i);
    }
    let ci: Vec<usize> = columns.iter().enumerate().filter(|(_, c)| c.as_ref().to_lowercase() == name.to_lowercase()).map(|(i, _)| i).collect();
    match ci.len() {
        1 => Ok(ci[0]),
        0 => {
            let suggestion = closest_name(name, columns.iter().map(|c| c.as_ref()));
            let msg = match suggestion {
                Some(s) => trf("Unknown column '{}' (did you mean '{}'?)", &[&name, &s]),
                None => trf("Unknown column '{}'", &[&name]),
            };
            Err(ExprError::new(msg, span.clone()))
        }
        _ => Err(ExprError::new(trf("Column name '{}' is ambiguous; use the exact spelling", &[&name]), span.clone())),
    }
}

/// Closest candidate by edit distance (at most a third of the name length).
pub(crate) fn closest_name<'a>(name: &str, candidates: impl Iterator<Item = &'a str>) -> Option<String> {
    let target = name.to_lowercase();
    let limit = (target.chars().count() / 3).max(1);
    candidates.map(|c| (levenshtein(&target, &c.to_lowercase()), c)).filter(|(d, _)| *d <= limit).min_by_key(|(d, _)| *d).map(|(_, c)| c.to_string())
}

/// Optimal string alignment distance (Levenshtein with adjacent transpositions).
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = v;
        }
    }
    d[n][m]
}

fn bind<S: AsRef<str>>(ast: Ast, columns: &[S]) -> Result<Node, ExprError> {
    Ok(match ast {
        Ast::Literal(v) => Node::Literal(v),
        Ast::Column { name, span } => Node::Column(resolve_column(&name, columns, &span)?),
        Ast::Unary(op, e) => Node::Unary(op, Box::new(bind(*e, columns)?)),
        Ast::Binary(op, l, r) => Node::Binary(op, Box::new(bind(*l, columns)?), Box::new(bind(*r, columns)?)),
        Ast::IsNull { expr, negated } => Node::IsNull { expr: Box::new(bind(*expr, columns)?), negated },
        Ast::Like { expr, pattern, negated, case_insensitive } => {
            Node::Like { expr: Box::new(bind(*expr, columns)?), pattern: Box::new(bind(*pattern, columns)?), negated, case_insensitive }
        }
        Ast::In { expr, list, negated } => Node::In { expr: Box::new(bind(*expr, columns)?), list: list.into_iter().map(|a| bind(a, columns)).collect::<Result<_, _>>()?, negated },
        Ast::Between { expr, low, high, negated } => {
            Node::Between { expr: Box::new(bind(*expr, columns)?), low: Box::new(bind(*low, columns)?), high: Box::new(bind(*high, columns)?), negated }
        }
        Ast::Call { name, args, span } => {
            let Some(func) = Func::lookup(&name) else {
                let suggestion = closest_name(&name, Func::ALL.iter().map(|(_, n)| *n));
                let msg = match suggestion {
                    Some(s) => trf("Unknown function '{}' (did you mean '{}'?)", &[&name, &s]),
                    None => trf("Unknown function '{}'", &[&name]),
                };
                return Err(ExprError::new(msg, span));
            };
            let (min, max) = func.arity();
            if args.len() < min || args.len() > max {
                let expected = if min == max {
                    min.to_string()
                } else if max == usize::MAX {
                    trf("at least {}", &[&min])
                } else {
                    trf("{} to {}", &[&min, &max])
                };
                return Err(ExprError::new(trf("{}() takes {} argument(s), got {}", &[&func.name(), &expected, &args.len()]), span));
            }
            Node::Call(func, args.into_iter().map(|a| bind(a, columns)).collect::<Result<_, _>>()?)
        }
    })
}

/// Short reference of the language, shown as help text in the UI.
pub fn help_text() -> String {
    tr("Columns: name, [any name] or \"any name\". Literals: 12, 1.5, 'text', TRUE, FALSE, NULL.\n\
Operators: + - * / %  ||  = <> < <= > >=  AND OR NOT  IS [NOT] NULL  [NOT] LIKE/ILIKE  [NOT] IN (...)  BETWEEN a AND b.\n\
Functions: upper lower length substr trim ltrim rtrim replace left right round abs coalesce concat if nullif to_number to_text.")
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLS: [&str; 5] = ["id", "name", "price", "Qty", "created at"];

    fn ev(src: &str, row: &[Value]) -> Result<Value, EvalError> {
        Expr::compile(src, &COLS).unwrap().eval(row)
    }

    fn row() -> Vec<Value> {
        vec![Value::Int(7), Value::text("Widget"), Value::decimal("2.50"), Value::Int(4), Value::Null]
    }

    #[test]
    fn compile_errors() {
        let e = Expr::compile("nmae = 'x'", &COLS).unwrap_err();
        assert!(e.message.contains("did you mean 'name'"), "{}", e.message);
        assert_eq!(e.span, 0..4);
        assert_eq!(e.display_with_position("nmae = 'x'"), format!("{} at position 1", e.message));
        let e = Expr::compile("uper(name)", &COLS).unwrap_err();
        assert!(e.message.contains("upper"), "{}", e.message);
        let e = Expr::compile("substr(name)", &COLS).unwrap_err();
        assert!(e.message.contains("2 to 3"), "{}", e.message);
        let e = Expr::compile("coalesce()", &COLS).unwrap_err();
        assert!(e.message.contains("at least 1"), "{}", e.message);
        // case-insensitive resolution
        assert!(Expr::compile("qty > 1", &COLS).is_ok());
        assert!(Expr::compile("[created at] is null", &COLS).is_ok());
        let ambiguous = ["a", "A1", "x", "X"];
        assert!(Expr::compile("x", &ambiguous).is_ok());
        assert!(Expr::compile("[a1]", &ambiguous).is_ok());
        let ambiguous2 = ["Col", "COL"];
        assert!(Expr::compile("col", &ambiguous2).is_err());
    }

    #[test]
    fn arithmetic_and_text() {
        let r = row();
        assert_eq!(ev("price * qty", &r).unwrap(), Value::Float(10.0));
        assert_eq!(ev("id + qty", &r).unwrap(), Value::Int(11));
        assert_eq!(ev("id - 10", &r).unwrap(), Value::Int(-3));
        assert_eq!(ev("id / 2", &r).unwrap(), Value::Float(3.5));
        assert_eq!(ev("8 / 2", &r).unwrap(), Value::Int(4));
        assert_eq!(ev("id % 4", &r).unwrap(), Value::Int(3));
        assert_eq!(ev("-id", &r).unwrap(), Value::Int(-7));
        assert!(ev("id / 0", &r).is_err());
        assert!(ev("id % 0", &r).is_err());
        assert_eq!(ev("9223372036854775807 + 1", &r).unwrap(), Value::Float(9223372036854775808.0));
        assert_eq!(ev("name || '-' || id", &r).unwrap(), Value::text("Widget-7"));
        assert_eq!(ev("name || [created at]", &r).unwrap(), Value::Null);
        assert_eq!(ev("[created at] + 1", &r).unwrap(), Value::Null);
        assert_eq!(ev("'3' + 4", &r).unwrap(), Value::Int(7));
        assert!(ev("name + 1", &r).is_err());
    }

    #[test]
    fn functions() {
        let r = row();
        assert_eq!(ev("upper(name)", &r).unwrap(), Value::text("WIDGET"));
        assert_eq!(ev("lower(name)", &r).unwrap(), Value::text("widget"));
        assert_eq!(ev("length(name)", &r).unwrap(), Value::Int(6));
        assert_eq!(ev("length('héllo')", &r).unwrap(), Value::Int(5));
        assert_eq!(ev("substr(name, 2, 3)", &r).unwrap(), Value::text("idg"));
        assert_eq!(ev("substr(name, 4)", &r).unwrap(), Value::text("get"));
        assert_eq!(ev("substr(name, 0, 2)", &r).unwrap(), Value::text("W"));
        assert_eq!(ev("substr(name, 10)", &r).unwrap(), Value::text(""));
        assert_eq!(ev("trim('  a b  ')", &r).unwrap(), Value::text("a b"));
        assert_eq!(ev("ltrim('  a ')", &r).unwrap(), Value::text("a "));
        assert_eq!(ev("rtrim('  a ')", &r).unwrap(), Value::text("  a"));
        assert_eq!(ev("replace(name, 'dg', 'DG')", &r).unwrap(), Value::text("WiDGet"));
        assert_eq!(ev("left(name, 3)", &r).unwrap(), Value::text("Wid"));
        assert_eq!(ev("right(name, 3)", &r).unwrap(), Value::text("get"));
        assert_eq!(ev("round(2.345, 2)", &r).unwrap(), Value::Float(2.35));
        assert_eq!(ev("round(price)", &r).unwrap(), Value::decimal("3"));
        assert_eq!(ev("round(1234, -2)", &r).unwrap(), Value::Int(1200));
        assert_eq!(ev("abs(-3)", &r).unwrap(), Value::Int(3));
        assert_eq!(ev("abs(-2.5)", &r).unwrap(), Value::Float(2.5));
        assert_eq!(ev("coalesce([created at], name)", &r).unwrap(), Value::text("Widget"));
        assert_eq!(ev("concat(name, [created at], id)", &r).unwrap(), Value::text("Widget7"));
        assert_eq!(ev("if(qty > 3, 'many', 'few')", &r).unwrap(), Value::text("many"));
        assert_eq!(ev("if(null, 1, 2)", &r).unwrap(), Value::Int(2));
        assert_eq!(ev("nullif(qty, 4)", &r).unwrap(), Value::Null);
        assert_eq!(ev("nullif(qty, 5)", &r).unwrap(), Value::Int(4));
        assert_eq!(ev("to_number('12')", &r).unwrap(), Value::Int(12));
        assert_eq!(ev("to_number(' 1.5 ')", &r).unwrap(), Value::Float(1.5));
        assert_eq!(ev("to_number(price)", &r).unwrap(), Value::decimal("2.50"));
        assert_eq!(ev("to_number('')", &r).unwrap(), Value::Null);
        assert!(ev("to_number('abc')", &r).is_err());
        assert_eq!(ev("to_text(id)", &r).unwrap(), Value::text("7"));
        assert_eq!(ev("to_text(null)", &r).unwrap(), Value::Null);
        assert_eq!(ev("upper(null)", &r).unwrap(), Value::Null);
        assert_eq!(ev("IFNULL([created at], 0)", &r).unwrap(), Value::Int(0));
    }

    #[test]
    fn predicates_and_logic() {
        let r = row();
        let t = |s: &str| Expr::compile(s, &COLS).unwrap().matches(&r[..]);
        assert!(t("id = 7"));
        assert!(t("id == 7.0"));
        assert!(t("price = 2.5"));
        assert!(t("price > 2"));
        assert!(t("name = 'Widget'"));
        assert!(!t("name = 'widget'"));
        assert!(t("name <> 'x'"));
        assert!(t("id >= '7'"));
        assert!(t("name like 'Wid%'"));
        assert!(t("name like '_idget'"));
        assert!(!t("name like 'wid%'"));
        assert!(t("name ilike 'wid%'"));
        assert!(t("name not like '%x%'"));
        assert!(t("[created at] is null"));
        assert!(!t("[created at] is not null"));
        assert!(!t("[created at] = 1"));
        assert!(!t("not [created at] = 1"));
        assert!(t("id in (1, 7, 9)"));
        assert!(t("id not in (1, 2)"));
        assert!(!t("id not in (1, null)"));
        assert!(t("qty between 1 and 4"));
        assert!(!t("qty not between 1 and 4"));
        assert!(t("id = 7 and (qty = 1 or qty = 4)"));
        assert!(t("[created at] = 1 or id = 7"));
        assert!(!t("[created at] = 1 and id = 7"));
        assert!(t("not ([created at] = 1 and id = 8)"));
        assert!(t("true"));
        assert!(!t("null"));
        assert!(!t("id / 0 = 1"));
    }

    #[test]
    fn dates_compare_with_text() {
        let d = chrono::NaiveDate::from_ymd_opt(2024, 5, 6).unwrap();
        let cols = ["d", "ts"];
        let r = [Value::Date(d), Value::DateTime(d.and_hms_opt(10, 0, 0).unwrap())];
        let t = |s: &str| Expr::compile(s, &cols).unwrap().matches(&r[..]);
        assert!(t("d = '2024-05-06'"));
        assert!(t("d > '2024-01-01'"));
        assert!(t("ts >= '2024-05-06'"));
        assert!(t("ts < '2024-05-06 11:00:00'"));
        assert!(t("d < ts"));
    }

    #[test]
    fn referenced_columns() {
        let e = Expr::compile("price * qty + coalesce(id, price)", &COLS).unwrap();
        assert_eq!(e.referenced_columns(), vec![0, 2, 3]);
    }

    #[test]
    fn never_panics_on_overflow_or_hand_built_trees() {
        let r = row();
        let min = "(-9223372036854775807 - 1)";
        assert_eq!(ev(&format!("{min} / -1"), &r).unwrap(), Value::Float(9223372036854775808.0));
        assert!(ev(&format!("{min} % -1"), &r).is_ok());
        assert_eq!(ev(&format!("{min} * 2"), &r).unwrap(), Value::Float(-18446744073709551616.0));
        assert_eq!(ev(&format!("-({min})"), &r).unwrap(), Value::Float(9223372036854775808.0));
        assert_eq!(ev(&format!("abs({min})"), &r).unwrap(), Value::Float(9223372036854775808.0));
        assert!(ev("round(1e300, -18)", &r).is_ok());
        let bad = Expr::from_node(Node::Call(Func::If, vec![]), "if()");
        assert!(bad.eval(&r[..]).is_err());
        let bad = Expr::from_node(Node::Call(Func::NullIf, vec![Node::Literal(Value::Int(1))]), "nullif(1)");
        assert!(bad.eval(&r[..]).is_err());
        // Deeply nested input is rejected by the parser instead of overflowing the stack.
        let deep = "not ".repeat(10_000) + "true";
        assert!(Expr::compile(&deep, &COLS).is_err());
    }
}
