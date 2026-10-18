//! Query parameters and template variables (FR-S5).
//!
//! * `${name}` template variables are substituted everywhere except in
//!   comments, including inside string literals and quoted identifiers.
//! * `:name` named parameters, `?` positional parameters and `$1` numbered
//!   parameters are recognised only outside strings and comments; `::` casts,
//!   `:=` assignments and `a[1:n]` slices are not parameters.
//!
//! Substituted values are SQL text inserted verbatim (DBeaver semantics):
//! callers quote string values themselves.

use crate::token::{TokenKind, tokenize};
use dbiji_core::SqlSyntax;
use serde::{Deserialize, Serialize};

/// Parameter syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ParamKind {
    /// `:name`
    Named,
    /// `?` (named "1", "2", ... in order of appearance)
    Positional,
    /// `$1`, `?1`
    Numbered,
    /// `${name}`
    Template,
}

/// One occurrence of a parameter.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParamRef {
    /// Syntax of the parameter.
    pub kind: ParamKind,
    /// Name without prefix: `name` for `:name` / `${name}`, `1` for `$1`,
    /// the 1-based ordinal for `?`.
    pub name: String,
    /// Byte range of the whole placeholder (`:name`, `${name}`, `?`).
    pub start: usize,
    /// End of the placeholder (exclusive).
    pub end: usize,
}

/// Which parameter syntaxes are enabled (user settings).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct ParamOptions {
    /// `:name`
    pub named: bool,
    /// `${name}`
    pub template: bool,
    /// Anonymous `?` (also when the dialect uses `?` as an operator).
    pub positional: bool,
    /// `$1` / `?1`
    pub numbered: bool,
}

impl Default for ParamOptions {
    fn default() -> Self {
        ParamOptions {
            named: true,
            template: true,
            positional: false,
            numbered: false,
        }
    }
}

fn is_template_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-')
}

/// `${name}` occurrences inside `text` (which starts at byte `base`).
fn templates_in(text: &str, base: usize, out: &mut Vec<ParamRef>) {
    let b = text.as_bytes();
    let mut i = 0;
    while i + 2 < b.len() {
        if b[i] == b'$' && b[i + 1] == b'{' {
            let mut j = i + 2;
            while j < b.len() && is_template_char(b[j]) {
                j += 1;
            }
            if j < b.len() && b[j] == b'}' && j > i + 2 {
                out.push(ParamRef {
                    kind: ParamKind::Template,
                    name: text[i + 2..j].to_string(),
                    start: base + i,
                    end: base + j + 1,
                });
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
}

/// All parameter occurrences in `src`, in text order.
pub fn find_parameters(src: &str, syntax: &SqlSyntax, opts: &ParamOptions) -> Vec<ParamRef> {
    let mut out = Vec::new();
    let mut positional = 0usize;
    for t in tokenize(src, syntax) {
        let text = t.text(src);
        match t.kind {
            TokenKind::LineComment | TokenKind::BlockComment => {}
            TokenKind::Variable if text.starts_with("${") => {
                if opts.template && !t.unterminated && text.len() > 3 {
                    out.push(ParamRef {
                        kind: ParamKind::Template,
                        name: text[2..text.len() - 1].to_string(),
                        start: t.start,
                        end: t.end,
                    });
                }
            }
            TokenKind::String | TokenKind::QuotedIdentifier | TokenKind::ClientCommand => {
                if opts.template {
                    templates_in(text, t.start, &mut out);
                }
            }
            TokenKind::Parameter => {
                let (kind, name) = if let Some(n) = text.strip_prefix(':') {
                    (ParamKind::Named, n.to_string())
                } else if text == "?" {
                    (ParamKind::Positional, String::new())
                } else if let Some(n) = text.strip_prefix('?').or_else(|| text.strip_prefix('$')) {
                    (ParamKind::Numbered, n.to_string())
                } else {
                    continue;
                };
                let enabled = match kind {
                    ParamKind::Named => opts.named,
                    ParamKind::Positional => opts.positional,
                    ParamKind::Numbered => opts.numbered,
                    ParamKind::Template => opts.template,
                };
                if !enabled {
                    continue;
                }
                let name = if kind == ParamKind::Positional {
                    positional += 1;
                    positional.to_string()
                } else {
                    name
                };
                out.push(ParamRef {
                    kind,
                    name,
                    start: t.start,
                    end: t.end,
                });
            }
            TokenKind::Operator if text == "?" && opts.positional => {
                positional += 1;
                out.push(ParamRef {
                    kind: ParamKind::Positional,
                    name: positional.to_string(),
                    start: t.start,
                    end: t.end,
                });
            }
            _ => {}
        }
    }
    out
}

/// Distinct parameter names in order of first appearance (for the
/// parameter panel), with their kind.
pub fn parameter_names(refs: &[ParamRef]) -> Vec<(ParamKind, String)> {
    let mut seen: Vec<(ParamKind, String)> = Vec::new();
    for r in refs {
        if !seen.iter().any(|(k, n)| *k == r.kind && n == &r.name) {
            seen.push((r.kind, r.name.clone()));
        }
    }
    seen
}

/// Replace every parameter by the SQL text returned by `values`. When some
/// values are missing, returns every occurrence without a value.
pub fn substitute(
    src: &str,
    syntax: &SqlSyntax,
    opts: &ParamOptions,
    values: &dyn Fn(&ParamRef) -> Option<String>,
) -> Result<String, Vec<ParamRef>> {
    let refs = find_parameters(src, syntax, opts);
    let mut out = String::with_capacity(src.len());
    let mut missing = Vec::new();
    let mut last = 0;
    for r in refs {
        if r.start < last {
            continue;
        }
        match values(&r) {
            Some(v) => {
                out.push_str(&src[last..r.start]);
                out.push_str(&v);
                last = r.end;
            }
            None => missing.push(r),
        }
    }
    if !missing.is_empty() {
        return Err(missing);
    }
    out.push_str(&src[last..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    fn all() -> ParamOptions {
        ParamOptions {
            named: true,
            template: true,
            positional: true,
            numbered: true,
        }
    }

    #[test]
    fn finds_named_and_templates() {
        let src = "SELECT * FROM ${tbl} WHERE a = :a AND b = ':not' AND c::int = :c -- :d ${e}\n AND s = '${inside}'";
        let r = find_parameters(src, &pg(), &ParamOptions::default());
        let got: Vec<_> = r
            .iter()
            .map(|p| (p.kind, p.name.as_str(), &src[p.start..p.end]))
            .collect();
        assert_eq!(
            got,
            vec![
                (ParamKind::Template, "tbl", "${tbl}"),
                (ParamKind::Named, "a", ":a"),
                (ParamKind::Named, "c", ":c"),
                (ParamKind::Template, "inside", "${inside}"),
            ]
        );
    }

    #[test]
    fn positional_and_numbered() {
        let src = "SELECT ? , ?, $1, '?' FROM t WHERE x = ?";
        let r = find_parameters(src, &mysql(), &all());
        let got: Vec<_> = r.iter().map(|p| (p.kind, p.name.as_str())).collect();
        assert_eq!(
            got,
            vec![
                (ParamKind::Positional, "1"),
                (ParamKind::Positional, "2"),
                (ParamKind::Positional, "3")
            ]
        );
        let r = find_parameters("SELECT $1, $2, $1", &pg(), &all());
        assert_eq!(
            r.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            vec!["1", "2", "1"]
        );
        assert!(r.iter().all(|p| p.kind == ParamKind::Numbered));
        // Disabled kinds are ignored
        assert!(find_parameters("SELECT ?", &mysql(), &ParamOptions::default()).is_empty());
        // `?` operators become positional parameters only when enabled
        let r = find_parameters("SELECT a ? b", &pg(), &all());
        assert_eq!(r.len(), 1);
        assert!(find_parameters("SELECT a ? b", &pg(), &ParamOptions::default()).is_empty());
        // PG `?|` operator is never a parameter
        assert!(find_parameters("SELECT a ?| b", &pg(), &all()).is_empty());
    }

    #[test]
    fn not_parameters() {
        let r = find_parameters(
            "SELECT a::text, x := 1, arr[1:n], \"q:x\", $$ :y $$",
            &pg(),
            &all(),
        );
        assert!(r.is_empty(), "{r:?}");
    }

    #[test]
    fn names_and_substitution() {
        let src = "SELECT :a, :b, :a, ${t}";
        let r = find_parameters(src, &pg(), &ParamOptions::default());
        assert_eq!(
            parameter_names(&r),
            vec![
                (ParamKind::Named, "a".into()),
                (ParamKind::Named, "b".into()),
                (ParamKind::Template, "t".into())
            ]
        );
        let out = substitute(
            src,
            &pg(),
            &ParamOptions::default(),
            &|p| match p.name.as_str() {
                "a" => Some("1".into()),
                "b" => Some("'x'".into()),
                "t" => Some("users".into()),
                _ => None,
            },
        );
        assert_eq!(out.unwrap(), "SELECT 1, 'x', 1, users");
        let err = substitute(src, &pg(), &ParamOptions::default(), &|p| {
            (p.name == "a").then(|| "1".to_string())
        });
        let missing = err.unwrap_err();
        assert_eq!(
            missing.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            vec!["b", "t"]
        );
        let out = substitute("SELECT ?, ?", &mysql(), &all(), &|p| {
            Some(format!("v{}", p.name))
        });
        assert_eq!(out.unwrap(), "SELECT v1, v2");
        let out = substitute(
            "x 'a ${v} b' /* ${v} */",
            &pg(),
            &ParamOptions::default(),
            &|_| Some("Z".into()),
        );
        assert_eq!(out.unwrap(), "x 'a Z b' /* ${v} */");
        assert_eq!(substitute("", &pg(), &all(), &|_| None).unwrap(), "");
    }

    #[test]
    fn templates_in_identifiers_and_commands() {
        let src = "@include ${dir}/x.sql\nSELECT * FROM log_${suffix}";
        let r = find_parameters(src, &pg(), &ParamOptions::default());
        assert_eq!(
            r.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            vec!["dir", "suffix"]
        );
    }

    #[test]
    fn random_inputs_never_panic() {
        let mut rng = Rng::new(5);
        for _ in 0..300 {
            let src = random_sql(&mut rng, 30);
            for syn in all_syntaxes() {
                let refs = find_parameters(&src, &syn, &all());
                for r in &refs {
                    assert!(
                        src.is_char_boundary(r.start)
                            && src.is_char_boundary(r.end)
                            && r.start < r.end
                    );
                }
                let _ = substitute(&src, &syn, &all(), &|p| Some(p.name.clone()));
            }
        }
    }
}
