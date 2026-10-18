//! Client side parameter binding: ClickHouse's HTTP interface has no
//! positional binding protocol, so `?` placeholders are replaced with SQL
//! literals rendered by the dialect. Placeholders inside string literals,
//! quoted identifiers and comments are left alone.

use dbiji_core::dialect::Dialect;
use dbiji_core::{DbError, DbResult, Value};

/// Replace each `?` placeholder of `sql` with the literal of the matching
/// value. Errors when the number of placeholders differs from `params.len()`.
pub fn bind(sql: &str, params: &[Value], dialect: &dyn Dialect) -> DbResult<String> {
    if params.is_empty() {
        return Ok(sql.to_string());
    }
    let mut out = String::with_capacity(sql.len() + params.len() * 8);
    let mut used = 0usize;
    let mut chars = sql.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '\'' | '`' | '"' => {
                out.push(c);
                // copy the quoted text verbatim, honouring backslash escapes
                // and doubled quotes
                while let Some((_, d)) = chars.next() {
                    out.push(d);
                    if d == '\\' {
                        if let Some((_, e)) = chars.next() {
                            out.push(e);
                        }
                    } else if d == c {
                        if chars.peek().is_some_and(|(_, n)| *n == c) {
                            if let Some((_, n)) = chars.next() {
                                out.push(n);
                            }
                        } else {
                            break;
                        }
                    }
                }
            }
            '-' if sql[i..].starts_with("--") => copy_line(&mut out, &mut chars, c),
            '#' => copy_line(&mut out, &mut chars, c),
            '/' if sql[i..].starts_with("/*") => {
                out.push(c);
                let mut prev = '\0';
                for (_, d) in chars.by_ref() {
                    out.push(d);
                    if prev == '*' && d == '/' {
                        break;
                    }
                    prev = d;
                }
            }
            '?' => {
                let Some(v) = params.get(used) else {
                    return Err(DbError::config(format!(
                        "The statement has more '?' placeholders than the {} bound parameters",
                        params.len()
                    )));
                };
                out.push_str(&dialect.literal(v, None));
                used += 1;
            }
            c => out.push(c),
        }
    }
    if used != params.len() {
        return Err(DbError::config(format!(
            "The statement has {used} '?' placeholders but {} parameters were bound",
            params.len()
        )));
    }
    Ok(out)
}

fn copy_line(out: &mut String, chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>, first: char) {
    out.push(first);
    for (_, d) in chars.by_ref() {
        out.push(d);
        if d == '\n' {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::ClickHouseDialect;

    #[test]
    fn binds_values() {
        let d = ClickHouseDialect::new();
        let sql = bind(
            "SELECT * FROM t WHERE a = ? AND b = '?' AND `c?` = ? -- ?\n AND d = ? /* ? */",
            &[Value::Int(1), Value::text("it's"), Value::Null],
            &d,
        )
        .unwrap();
        assert_eq!(sql, "SELECT * FROM t WHERE a = 1 AND b = '?' AND `c?` = 'it\\'s' -- ?\n AND d = NULL /* ? */");
    }

    #[test]
    fn escapes_inside_strings() {
        let d = ClickHouseDialect::new();
        let sql = bind("SELECT 'a\\'?', ?", &[Value::Int(5)], &d).unwrap();
        assert_eq!(sql, "SELECT 'a\\'?', 5");
        let sql = bind("SELECT 'a''?', ?", &[Value::Int(5)], &d).unwrap();
        assert_eq!(sql, "SELECT 'a''?', 5");
    }

    #[test]
    fn count_mismatch() {
        let d = ClickHouseDialect::new();
        assert!(bind("SELECT ?, ?", &[Value::Int(1)], &d).is_err());
        assert!(bind("SELECT 1", &[Value::Int(1)], &d).is_err());
        assert_eq!(bind("SELECT 1 ? 2 : 3", &[], &d).unwrap(), "SELECT 1 ? 2 : 3");
    }
}
