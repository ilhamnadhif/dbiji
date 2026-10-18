//! Expression evaluator with SQL-like NULL semantics.

use super::parser::{BinaryOp, UnaryOp};
use super::{EvalError, Func, Node};
use dbiji_core::value::{parse_bool, parse_date, parse_datetime, parse_datetime_tz, parse_time};
use dbiji_core::{Value, tr, trf};
use std::borrow::Cow;
use std::cmp::Ordering;

/// Read access to the values of one row.
pub trait RowValues {
    /// Value of column `col`; `None` when the column does not exist (treated as NULL).
    fn value(&self, col: usize) -> Option<&Value>;
}

impl RowValues for [Value] {
    fn value(&self, col: usize) -> Option<&Value> {
        self.get(col)
    }
}

impl RowValues for Vec<Value> {
    fn value(&self, col: usize) -> Option<&Value> {
        self.get(col)
    }
}

/// Truthiness used by filters: `TRUE`, non-zero numbers and true-like text.
pub fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Int(i) => *i != 0,
        Value::UInt(u) => *u != 0,
        Value::Float(f) => *f != 0.0 && !f.is_nan(),
        Value::Text(s) => parse_bool(s).unwrap_or(false),
        _ => false,
    }
}

/// Three-valued boolean.
fn to_bool3(v: &Value) -> Result<Option<bool>, EvalError> {
    match v {
        Value::Null => Ok(None),
        Value::Bool(b) => Ok(Some(*b)),
        Value::Int(_) | Value::UInt(_) | Value::Float(_) => Ok(Some(is_truthy(v))),
        Value::Text(s) => match parse_bool(s) {
            Some(b) => Ok(Some(b)),
            None => Err(EvalError(trf("'{}' is not a boolean", &[&s]))),
        },
        other => Err(EvalError(trf("A {} value is not a boolean", &[&other.variant_name()]))),
    }
}

fn bool3(b: Option<bool>) -> Value {
    b.map_or(Value::Null, Value::Bool)
}

/// Numeric view of a value used by arithmetic.
#[derive(Debug, Clone, Copy)]
enum Num {
    Int(i64),
    Float(f64),
}

fn to_num(v: &Value) -> Option<Num> {
    match v {
        Value::Int(i) => Some(Num::Int(*i)),
        Value::UInt(u) => Some(match i64::try_from(*u) {
            Ok(i) => Num::Int(i),
            Err(_) => Num::Float(*u as f64),
        }),
        Value::Float(f) => Some(Num::Float(*f)),
        Value::Bool(b) => Some(Num::Int(*b as i64)),
        Value::Decimal(s) => s.trim().parse::<f64>().ok().map(Num::Float),
        Value::Text(s) => {
            let t = s.trim();
            if let Ok(i) = t.parse::<i64>() {
                Some(Num::Int(i))
            } else {
                t.parse::<f64>().ok().filter(|f| f.is_finite()).map(Num::Float)
            }
        }
        _ => None,
    }
}

fn num_value(n: Num) -> Value {
    match n {
        Num::Int(i) => Value::Int(i),
        Num::Float(f) => Value::Float(f),
    }
}

fn as_f64(n: Num) -> f64 {
    match n {
        Num::Int(i) => i as f64,
        Num::Float(f) => f,
    }
}

fn is_numeric_value(v: &Value) -> bool {
    matches!(v, Value::Int(_) | Value::UInt(_) | Value::Float(_) | Value::Decimal(_))
}

/// Compare two values for filtering. Returns `None` when either is NULL.
///
/// Numbers compare numerically (also against numeric text), temporal values
/// against text that parses as a date/time, everything else by text.
pub fn compare_values(a: &Value, b: &Value) -> Option<Ordering> {
    if a.is_null() || b.is_null() {
        return None;
    }
    if (is_numeric_value(a) || is_numeric_value(b))
        && let (Some(x), Some(y)) = (to_num(a), to_num(b))
    {
        return Some(match (x, y) {
            (Num::Int(x), Num::Int(y)) => x.cmp(&y),
            (x, y) => {
                let (x, y) = (as_f64(x), as_f64(y));
                x.partial_cmp(&y).unwrap_or_else(|| x.is_nan().cmp(&y.is_nan()))
            }
        });
    }
    match (a, b) {
        (Value::Text(s), other) if !matches!(other, Value::Text(_)) => Some(match coerce_text_to(s, other) {
            Some(parsed) => parsed.cmp_for_sort(other),
            None => s.as_ref().cmp(other.to_text().as_ref()),
        }),
        (other, Value::Text(s)) if !matches!(other, Value::Text(_)) => Some(match coerce_text_to(s, other) {
            Some(parsed) => other.cmp_for_sort(&parsed),
            None => other.to_text().as_ref().cmp(s.as_ref()),
        }),
        _ => Some(a.cmp_for_sort(b)),
    }
}

/// Convert text to the variant of `like` so it can be compared with it.
/// Returns `None` when the kinds are compared as text.
fn coerce_text_to(s: &str, like: &Value) -> Option<Value> {
    let t = s.trim();
    match like {
        Value::Date(_) => parse_date(t).or_else(|| parse_datetime(t).map(|d| d.date())).map(Value::Date),
        Value::DateTime(_) => parse_datetime(t).map(Value::DateTime),
        Value::DateTimeTz(_) => parse_datetime_tz(t).map(Value::DateTimeTz),
        Value::Time(_) => parse_time(t).map(Value::Time),
        Value::Bool(_) => parse_bool(t).map(Value::Bool),
        Value::Uuid(_) => uuid_parse(t),
        _ => None,
    }
}

fn uuid_parse(t: &str) -> Option<Value> {
    // Avoid a direct uuid dependency: go through the core parser.
    Value::parse_as(t, dbiji_core::DataKind::Uuid).ok()
}

/// SQL `LIKE` matching: `%` matches any sequence, `_` one character and
/// `\` escapes the next character.
pub fn like_match(text: &str, pattern: &str, case_insensitive: bool) -> bool {
    let fold = |s: &str| -> Vec<char> {
        if case_insensitive { s.chars().flat_map(char::to_lowercase).collect() } else { s.chars().collect() }
    };
    let t = fold(text);
    // Tokenise the pattern: Some(c) literal, None for '_', and a separate marker for '%'.
    #[derive(Clone, Copy, PartialEq)]
    enum P {
        Lit(char),
        One,
        Any,
    }
    let mut p = Vec::new();
    let mut chars = fold(pattern).into_iter();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(n) => p.push(P::Lit(n)),
                None => p.push(P::Lit('\\')),
            },
            '%' => {
                if p.last() != Some(&P::Any) {
                    p.push(P::Any);
                }
            }
            '_' => p.push(P::One),
            c => p.push(P::Lit(c)),
        }
    }
    let (mut ti, mut pi) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == P::One || p[pi] == P::Lit(t[ti])) {
            ti += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == P::Any {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == P::Any {
        pi += 1;
    }
    pi == p.len()
}

pub(crate) fn eval<R: RowValues + ?Sized>(node: &Node, row: &R) -> Result<Value, EvalError> {
    match node {
        Node::Literal(v) => Ok(v.clone()),
        Node::Column(c) => Ok(row.value(*c).cloned().unwrap_or(Value::Null)),
        Node::Unary(UnaryOp::Neg, e) => {
            let v = eval(e, row)?;
            if v.is_null() {
                return Ok(Value::Null);
            }
            if let Value::Decimal(s) = &v {
                let t = s.trim();
                return Ok(Value::decimal(match t.strip_prefix('-') {
                    Some(rest) => rest.to_string(),
                    None => format!("-{}", t.strip_prefix('+').unwrap_or(t)),
                }));
            }
            match to_num(&v) {
                Some(Num::Int(i)) => Ok(i.checked_neg().map_or(Value::Float(-(i as f64)), Value::Int)),
                Some(Num::Float(f)) => Ok(Value::Float(-f)),
                None => Err(EvalError(trf("Cannot negate a {} value", &[&v.variant_name()]))),
            }
        }
        Node::Unary(UnaryOp::Not, e) => {
            let v = eval(e, row)?;
            Ok(bool3(to_bool3(&v)?.map(|b| !b)))
        }
        Node::Binary(BinaryOp::And, l, r) => {
            let a = to_bool3(&eval(l, row)?)?;
            if a == Some(false) {
                return Ok(Value::Bool(false));
            }
            let b = to_bool3(&eval(r, row)?)?;
            Ok(bool3(match (a, b) {
                (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            }))
        }
        Node::Binary(BinaryOp::Or, l, r) => {
            let a = to_bool3(&eval(l, row)?)?;
            if a == Some(true) {
                return Ok(Value::Bool(true));
            }
            let b = to_bool3(&eval(r, row)?)?;
            Ok(bool3(match (a, b) {
                (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            }))
        }
        Node::Binary(op, l, r) => {
            let a = eval(l, row)?;
            let b = eval(r, row)?;
            binary(*op, &a, &b)
        }
        Node::IsNull { expr, negated } => {
            let v = eval(expr, row)?;
            Ok(Value::Bool(v.is_null() != *negated))
        }
        Node::Like { expr, pattern, negated, case_insensitive } => {
            let v = eval(expr, row)?;
            let p = eval(pattern, row)?;
            if v.is_null() || p.is_null() {
                return Ok(Value::Null);
            }
            let m = like_match(&v.to_text(), &p.to_text(), *case_insensitive);
            Ok(Value::Bool(m != *negated))
        }
        Node::In { expr, list, negated } => {
            let v = eval(expr, row)?;
            if v.is_null() {
                return Ok(Value::Null);
            }
            let mut saw_null = false;
            for item in list {
                let x = eval(item, row)?;
                match compare_values(&v, &x) {
                    Some(Ordering::Equal) => return Ok(Value::Bool(!*negated)),
                    None => saw_null = true,
                    _ => {}
                }
            }
            Ok(if saw_null { Value::Null } else { Value::Bool(*negated) })
        }
        Node::Between { expr, low, high, negated } => {
            let v = eval(expr, row)?;
            let lo = eval(low, row)?;
            let hi = eval(high, row)?;
            let ge = compare_values(&v, &lo).map(|o| o != Ordering::Less);
            let le = compare_values(&v, &hi).map(|o| o != Ordering::Greater);
            let inside = match (ge, le) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            };
            Ok(bool3(inside.map(|b| b != *negated)))
        }
        Node::Call(func, args) => call(*func, args, row),
    }
}

fn binary(op: BinaryOp, a: &Value, b: &Value) -> Result<Value, EvalError> {
    use BinaryOp::*;
    match op {
        Eq | NotEq | Lt | LtEq | Gt | GtEq => {
            let Some(ord) = compare_values(a, b) else { return Ok(Value::Null) };
            Ok(Value::Bool(match op {
                Eq => ord == Ordering::Equal,
                NotEq => ord != Ordering::Equal,
                Lt => ord == Ordering::Less,
                LtEq => ord != Ordering::Greater,
                Gt => ord == Ordering::Greater,
                _ => ord != Ordering::Less,
            }))
        }
        Concat => {
            if a.is_null() || b.is_null() {
                return Ok(Value::Null);
            }
            let mut s = a.to_text().into_owned();
            s.push_str(&b.to_text());
            Ok(Value::text(s))
        }
        Add | Sub | Mul | Div | Rem => {
            if a.is_null() || b.is_null() {
                return Ok(Value::Null);
            }
            let (Some(x), Some(y)) = (to_num(a), to_num(b)) else {
                let sym = match op {
                    Add => "+",
                    Sub => "-",
                    Mul => "*",
                    Div => "/",
                    _ => "%",
                };
                let bad = if to_num(a).is_none() { a } else { b };
                return Err(EvalError(trf(
                    "Operator {} needs numbers, got '{}'",
                    &[&sym, &truncate_for_message(&bad.to_text())],
                )));
            };
            arith(op, x, y)
        }
        And | Or => Err(EvalError(tr("Internal error: logical operator").to_string())),
    }
}

fn truncate_for_message(s: &str) -> String {
    if s.chars().count() > 40 {
        let mut t: String = s.chars().take(40).collect();
        t.push('…');
        t
    } else {
        s.to_string()
    }
}

fn arith(op: BinaryOp, x: Num, y: Num) -> Result<Value, EvalError> {
    use BinaryOp::*;
    if let (Num::Int(a), Num::Int(b)) = (x, y) {
        let r = match op {
            Add => a.checked_add(b),
            Sub => a.checked_sub(b),
            Mul => a.checked_mul(b),
            Div => {
                if b == 0 {
                    return Err(EvalError(tr("Division by zero").to_string()));
                }
                // `checked_rem` also guards `i64::MIN % -1`, which would overflow.
                match a.checked_rem(b) {
                    Some(0) => a.checked_div(b),
                    _ => return Ok(Value::Float(a as f64 / b as f64)),
                }
            }
            Rem => {
                if b == 0 {
                    return Err(EvalError(tr("Division by zero").to_string()));
                }
                a.checked_rem(b)
            }
            _ => None,
        };
        if let Some(r) = r {
            return Ok(Value::Int(r));
        }
    }
    let (a, b) = (as_f64(x), as_f64(y));
    let r = match op {
        Add => a + b,
        Sub => a - b,
        Mul => a * b,
        Div => {
            if b == 0.0 {
                return Err(EvalError(tr("Division by zero").to_string()));
            }
            a / b
        }
        Rem => {
            if b == 0.0 {
                return Err(EvalError(tr("Division by zero").to_string()));
            }
            a % b
        }
        _ => f64::NAN,
    };
    Ok(Value::Float(r))
}

fn text_arg<'a>(v: &'a Value) -> Cow<'a, str> {
    v.to_text()
}

fn int_arg(v: &Value, func: Func) -> Result<Option<i64>, EvalError> {
    if v.is_null() {
        return Ok(None);
    }
    match to_num(v) {
        Some(Num::Int(i)) => Ok(Some(i)),
        Some(Num::Float(f)) if f.is_finite() => Ok(Some(f.trunc() as i64)),
        _ => Err(EvalError(trf("{}() expects an integer argument", &[&func.name()]))),
    }
}

fn call<R: RowValues + ?Sized>(func: Func, args: &[Node], row: &R) -> Result<Value, EvalError> {
    // Arity is checked when binding parsed text, but trees can also be built
    // directly with `Expr::from_node`.
    let (min, max) = func.arity();
    if args.len() < min || args.len() > max {
        return Err(EvalError(trf("{}() takes {} argument(s), got {}", &[&func.name(), &min, &args.len()])));
    }
    // Lazily evaluated functions first.
    match func {
        Func::If => {
            let c = eval(&args[0], row)?;
            let cond = match to_bool3(&c) {
                Ok(b) => b.unwrap_or(false),
                Err(_) => is_truthy(&c),
            };
            return eval(if cond { &args[1] } else { &args[2] }, row);
        }
        Func::Coalesce => {
            for a in args {
                let v = eval(a, row)?;
                if !v.is_null() {
                    return Ok(v);
                }
            }
            return Ok(Value::Null);
        }
        _ => {}
    }
    let vals: Vec<Value> = args.iter().map(|a| eval(a, row)).collect::<Result<_, _>>()?;
    let first = vals.first().cloned().unwrap_or(Value::Null);
    match func {
        Func::Concat => {
            let mut s = String::new();
            for v in &vals {
                if !v.is_null() {
                    s.push_str(&v.to_text());
                }
            }
            Ok(Value::text(s))
        }
        Func::NullIf => Ok(match compare_values(&vals[0], &vals[1]) {
            Some(Ordering::Equal) => Value::Null,
            _ => first,
        }),
        Func::ToText => Ok(if first.is_null() { Value::Null } else { Value::text(first.to_text().into_owned()) }),
        Func::ToNumber => to_number(&first),
        _ if first.is_null() => Ok(Value::Null),
        Func::Upper => Ok(Value::text(text_arg(&first).to_uppercase())),
        Func::Lower => Ok(Value::text(text_arg(&first).to_lowercase())),
        Func::Length => Ok(Value::Int(match &first {
            Value::Bytes(b) => b.len() as i64,
            other => other.to_text().chars().count() as i64,
        })),
        Func::Trim => Ok(Value::text(text_arg(&first).trim())),
        Func::Ltrim => Ok(Value::text(text_arg(&first).trim_start())),
        Func::Rtrim => Ok(Value::text(text_arg(&first).trim_end())),
        Func::Replace => {
            if vals[1].is_null() || vals[2].is_null() {
                return Ok(Value::Null);
            }
            let from = text_arg(&vals[1]);
            if from.is_empty() {
                return Ok(Value::text(text_arg(&first).into_owned()));
            }
            Ok(Value::text(text_arg(&first).replace(from.as_ref(), &text_arg(&vals[2]))))
        }
        Func::Substr => {
            let s = text_arg(&first);
            let Some(start) = int_arg(&vals[1], func)? else { return Ok(Value::Null) };
            let len = match vals.get(2) {
                Some(v) => match int_arg(v, func)? {
                    Some(l) if l < 0 => return Err(EvalError(tr("substr() length must not be negative").to_string())),
                    Some(l) => Some(l),
                    None => return Ok(Value::Null),
                },
                None => None,
            };
            let chars: Vec<char> = s.chars().collect();
            let n = chars.len() as i64;
            let begin = start - 1;
            let end = match len {
                Some(l) => begin.saturating_add(l),
                None => n,
            };
            let (b, e) = (begin.clamp(0, n) as usize, end.clamp(0, n) as usize);
            Ok(Value::text(if b < e { chars[b..e].iter().collect::<String>() } else { String::new() }))
        }
        Func::Left | Func::Right => {
            let s = text_arg(&first);
            let Some(k) = int_arg(&vals[1], func)? else { return Ok(Value::Null) };
            let chars: Vec<char> = s.chars().collect();
            let n = chars.len();
            let k = if k < 0 { n.saturating_sub(k.unsigned_abs() as usize) } else { (k as usize).min(n) };
            Ok(Value::text(if func == Func::Left {
                chars[..k].iter().collect::<String>()
            } else {
                chars[n - k..].iter().collect::<String>()
            }))
        }
        Func::Abs => match &first {
            Value::Decimal(s) => Ok(Value::decimal(s.trim().trim_start_matches(['-', '+']).to_string())),
            other => match to_num(other) {
                Some(Num::Int(i)) => Ok(i.checked_abs().map_or(Value::Float((i as f64).abs()), Value::Int)),
                Some(Num::Float(f)) => Ok(Value::Float(f.abs())),
                None => Err(EvalError(tr("abs() expects a number").to_string())),
            },
        },
        Func::Round => {
            let digits = match vals.get(1) {
                Some(v) => match int_arg(v, func)? {
                    Some(d) => d.clamp(-18, 18) as i32,
                    None => return Ok(Value::Null),
                },
                None => 0,
            };
            round_value(&first, digits)
        }
        Func::If | Func::Coalesce => Ok(Value::Null),
    }
}

fn to_number(v: &Value) -> Result<Value, EvalError> {
    match v {
        Value::Null => Ok(Value::Null),
        Value::Int(_) | Value::UInt(_) | Value::Float(_) | Value::Decimal(_) => Ok(v.clone()),
        Value::Bool(b) => Ok(Value::Int(*b as i64)),
        other => {
            let t = other.to_text();
            let t = t.trim();
            if t.is_empty() {
                return Ok(Value::Null);
            }
            if let Ok(i) = t.parse::<i64>() {
                return Ok(Value::Int(i));
            }
            match t.parse::<f64>() {
                Ok(f) if f.is_finite() => Ok(Value::Float(f)),
                _ => Err(EvalError(trf("'{}' is not a number", &[&truncate_for_message(t)]))),
            }
        }
    }
}

fn round_value(v: &Value, digits: i32) -> Result<Value, EvalError> {
    match v {
        Value::Int(i) => {
            if digits >= 0 {
                return Ok(Value::Int(*i));
            }
            let p = 10i64.checked_pow(digits.unsigned_abs()).unwrap_or(i64::MAX);
            let r = (*i as i128 + if *i >= 0 { p as i128 / 2 } else { -(p as i128) / 2 }) / p as i128 * p as i128;
            Ok(i64::try_from(r).map_or(Value::Float(r as f64), Value::Int))
        }
        Value::UInt(_) => round_value(&Value::Float(v.as_f64().unwrap_or(0.0)), digits),
        Value::Decimal(s) => match round_decimal_text(s.trim(), digits) {
            Some(r) => Ok(Value::decimal(r)),
            None => Err(EvalError(tr("round() expects a number").to_string())),
        },
        Value::Float(f) => {
            if !f.is_finite() {
                return Ok(Value::Float(*f));
            }
            let text = format!("{f}");
            match round_decimal_text(&text, digits).and_then(|r| r.parse::<f64>().ok()) {
                Some(r) => Ok(Value::Float(r)),
                None => Ok(Value::Float(*f)),
            }
        }
        other => match to_num(other) {
            Some(n) => round_value(&num_value(n), digits),
            None => Err(EvalError(tr("round() expects a number").to_string())),
        },
    }
}

/// Round a plain decimal string (`-12.345`) half away from zero to `digits`
/// fraction digits (negative digits round to tens, hundreds ...).
pub(crate) fn round_decimal_text(s: &str, digits: i32) -> Option<String> {
    let (neg, body) = match s.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    if body.contains(['e', 'E']) {
        let f: f64 = s.parse().ok()?;
        return round_decimal_text(format!("{f:.20}").trim_end_matches('0').trim_end_matches('.'), digits)
            .filter(|_| f.is_finite());
    }
    let (int_part, frac_part) = match body.split_once('.') {
        Some((i, f)) => (i, f),
        None => (body, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.chars().all(|c| c.is_ascii_digit()) || !frac_part.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut all: Vec<u8> = int_part.bytes().chain(frac_part.bytes()).map(|b| b - b'0').collect();
    let int_len = int_part.len() as i64;
    // Number of digits kept from the start of `all`.
    let keep = int_len + digits as i64;
    if keep < 0 {
        return Some("0".to_string());
    }
    let keep = keep as usize;
    if keep < all.len() {
        let round_up = all[keep] >= 5;
        all.truncate(keep);
        let mut carry = round_up;
        let mut i = all.len();
        while carry && i > 0 {
            i -= 1;
            if all[i] == 9 {
                all[i] = 0;
            } else {
                all[i] += 1;
                carry = false;
            }
        }
        let mut int_digits = int_len as usize;
        if carry {
            all.insert(0, 1);
            int_digits += 1;
        }
        // Pad truncated integer positions with zeros (negative digits).
        while all.len() < int_digits {
            all.push(0);
        }
        let int_str: String = all[..int_digits.min(all.len())].iter().map(|d| (d + b'0') as char).collect();
        let frac_str: String = all[int_digits.min(all.len())..].iter().map(|d| (d + b'0') as char).collect();
        let int_str = {
            let t = int_str.trim_start_matches('0');
            if t.is_empty() { "0".to_string() } else { t.to_string() }
        };
        let is_zero = int_str == "0" && frac_str.chars().all(|c| c == '0');
        let mut out = String::new();
        if neg && !is_zero {
            out.push('-');
        }
        out.push_str(&int_str);
        if !frac_str.is_empty() {
            out.push('.');
            out.push_str(&frac_str);
        }
        Some(out)
    } else {
        Some(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_patterns() {
        assert!(like_match("hello", "h%", false));
        assert!(like_match("hello", "%llo", false));
        assert!(like_match("hello", "h_llo", false));
        assert!(like_match("hello", "%", false));
        assert!(like_match("", "%", false));
        assert!(!like_match("", "_", false));
        assert!(like_match("50%", "50\\%", false));
        assert!(!like_match("500", "50\\%", false));
        assert!(like_match("a_b", "a\\_b", false));
        assert!(!like_match("axb", "a\\_b", false));
        assert!(like_match("HeLLo", "hello", true));
        assert!(!like_match("HeLLo", "hello", false));
        assert!(like_match("abcabc", "%b%c", false));
        assert!(!like_match("abc", "a%d", false));
        assert!(like_match("ÄBC", "äb%", true));
    }

    #[test]
    fn decimal_rounding() {
        assert_eq!(round_decimal_text("2.345", 2).unwrap(), "2.35");
        assert_eq!(round_decimal_text("2.344", 2).unwrap(), "2.34");
        assert_eq!(round_decimal_text("-2.345", 2).unwrap(), "-2.35");
        assert_eq!(round_decimal_text("9.995", 2).unwrap(), "10.00");
        assert_eq!(round_decimal_text("0.5", 0).unwrap(), "1");
        assert_eq!(round_decimal_text("1234.5", -2).unwrap(), "1200");
        assert_eq!(round_decimal_text("1250", -2).unwrap(), "1300");
        assert_eq!(round_decimal_text("12", 2).unwrap(), "12");
        assert_eq!(round_decimal_text("-0.001", 2).unwrap(), "0.00");
        assert_eq!(round_decimal_text("5", -3).unwrap(), "0");
        assert_eq!(round_decimal_text("abc", 1), None);
    }

    #[test]
    fn compare_mixed() {
        assert_eq!(compare_values(&Value::Int(2), &Value::decimal("2.0")), Some(Ordering::Equal));
        assert_eq!(compare_values(&Value::text("10"), &Value::Int(9)), Some(Ordering::Greater));
        assert_eq!(compare_values(&Value::text("abc"), &Value::Int(9)), Some(Ordering::Greater));
        assert_eq!(compare_values(&Value::Null, &Value::Int(9)), None);
        assert_eq!(compare_values(&Value::text("b"), &Value::text("a")), Some(Ordering::Greater));
        assert_eq!(compare_values(&Value::Bool(true), &Value::text("yes")), Some(Ordering::Equal));
        assert_eq!(compare_values(&Value::UInt(u64::MAX), &Value::Int(-1)), Some(Ordering::Greater));
    }
}
