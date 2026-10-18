//! Script outline (FR-S6): statements with their CTEs, subqueries and
//! referenced tables.

use crate::analyze::{Model, RefRole, ScopeKind, SourceKind, build_model};
use crate::classify::{StatementKind, classify_tokens};
use crate::split::{SplitOptions, split_tokens};
use dbiji_core::{SqlSyntax, tr};
use serde::{Deserialize, Serialize};

/// What an outline node represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OutlineKind {
    /// A SQL statement.
    Statement,
    /// A client command line (`@set` ...).
    ClientCommand,
    /// A common table expression.
    Cte,
    /// A subquery or derived table.
    Subquery,
    /// A referenced table.
    Table,
}

/// One node of the outline tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineItem {
    /// Display text.
    pub label: String,
    /// Statement kind (`Select` for CTEs and subqueries, `Other` for tables).
    pub kind: StatementKind,
    /// What the node represents.
    pub item: OutlineKind,
    /// Byte range in the script.
    pub start: usize,
    /// End of the range (exclusive).
    pub end: usize,
    /// Nested nodes.
    pub children: Vec<OutlineItem>,
}

const LABEL_MAX: usize = 60;

/// Statement text with comments removed and white space collapsed, truncated.
fn summary(src: &str, tokens: &[crate::token::Token]) -> String {
    let mut s = String::new();
    for t in tokens {
        if t.kind.is_comment() {
            continue;
        }
        if t.is_trivia() {
            if !s.is_empty() && !s.ends_with(' ') {
                s.push(' ');
            }
            continue;
        }
        s.push_str(t.text(src));
        if s.chars().count() > LABEL_MAX + 10 {
            break;
        }
    }
    let s = s.trim().to_string();
    if s.chars().count() > LABEL_MAX {
        let cut: String = s.chars().take(LABEL_MAX).collect();
        format!("{}…", cut.trim_end())
    } else {
        s
    }
}

/// Outline of a script.
pub fn outline(src: &str, syntax: &SqlSyntax) -> Vec<OutlineItem> {
    let split = split_tokens(src, syntax, &SplitOptions::default());
    let mut out = Vec::with_capacity(split.statements.len());
    for st in &split.statements {
        let toks = split.tokens.get(st.tokens.clone()).unwrap_or(&[]);
        let info = classify_tokens(src, toks, syntax);
        let label = summary(src, toks);
        if st.span.is_client_command {
            out.push(OutlineItem {
                label,
                kind: StatementKind::ClientCommand,
                item: OutlineKind::ClientCommand,
                start: st.span.start,
                end: st.span.end,
                children: Vec::new(),
            });
            continue;
        }
        let m = build_model(src, toks, None, None, st.span.end);
        let mut children = Vec::new();
        for (i, s) in m.scopes.iter().enumerate() {
            // Top-level query blocks: the statement, its set-operation
            // branches and the parts of compound bodies.
            let top = s.parent.is_none() && matches!(s.kind, ScopeKind::Root | ScopeKind::Branch);
            if top {
                children.extend(scope_children(&m, i, 0));
            }
        }
        children.sort_by_key(|c| c.start);
        out.push(OutlineItem {
            label,
            kind: info.kind,
            item: OutlineKind::Statement,
            start: st.span.start,
            end: st.span.end,
            children,
        });
    }
    out
}

fn scope_children(m: &Model, scope: usize, depth: u32) -> Vec<OutlineItem> {
    let mut items = Vec::new();
    if depth > 16 {
        return items;
    }
    let Some(sc) = m.scopes.get(scope) else {
        return items;
    };
    for &c in &sc.ctes {
        let Some(cte) = m.ctes.get(c) else { continue };
        let (end, children) = match cte.body {
            Some(b) => (
                m.scopes.get(b).map(|s| s.end).unwrap_or(cte.name_end),
                scope_children(m, b, depth + 1),
            ),
            None => (cte.name_end, Vec::new()),
        };
        items.push(OutlineItem {
            label: cte.name.clone(),
            kind: StatementKind::Select,
            item: OutlineKind::Cte,
            start: cte.name_start,
            end: end.max(cte.name_end),
            children,
        });
    }
    for &si in &sc.sources {
        let s = &m.sources[si];
        match &s.kind {
            SourceKind::Table { .. } | SourceKind::Cte(_) => {
                if s.role == RefRole::Create {
                    continue;
                }
                let name: Vec<&str> = s.parts.iter().map(|p| p.0.as_str()).collect();
                let mut label = name.join(".");
                if let Some(a) = &s.alias {
                    label.push(' ');
                    label.push_str(a);
                }
                items.push(OutlineItem {
                    label,
                    kind: StatementKind::Other,
                    item: OutlineKind::Table,
                    start: s.start,
                    end: s.end,
                    children: Vec::new(),
                });
            }
            SourceKind::Derived(d) => {
                let label = match &s.alias {
                    Some(a) => format!("({}) {a}", tr("subquery")),
                    None => format!("({})", tr("subquery")),
                };
                let end = m.scopes.get(*d).map(|x| x.end).unwrap_or(s.end);
                items.push(OutlineItem {
                    label,
                    kind: StatementKind::Select,
                    item: OutlineKind::Subquery,
                    start: s.start,
                    end,
                    children: scope_children(m, *d, depth + 1),
                });
            }
            _ => {}
        }
    }
    for (i, child) in m.scopes.iter().enumerate() {
        if child.parent == Some(scope) && child.kind == ScopeKind::Subquery {
            items.push(OutlineItem {
                label: format!("({})", tr("subquery")),
                kind: StatementKind::Select,
                item: OutlineKind::Subquery,
                start: child.start,
                end: child.end,
                children: scope_children(m, i, depth + 1),
            });
        }
        // Set-operation branches of this block belong to the same level.
        if child.union_first == Some(scope) && i != scope && child.parent.is_some() {
            items.extend(scope_children(m, i, depth + 1));
        }
    }
    items.sort_by_key(|c| c.start);
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    #[test]
    fn statements_and_children() {
        let src = "-- report\nWITH x AS (SELECT * FROM orders) SELECT u.name FROM users u JOIN x ON x.user_id = u.id WHERE u.id IN (SELECT user_id FROM sales.customers);\n@set a = 1\nINSERT INTO logs (msg) VALUES ('x');";
        let o = outline(src, &pg());
        assert_eq!(o.len(), 3);
        assert_eq!(o[0].kind, StatementKind::Select);
        assert!(
            o[0].label.starts_with("WITH x AS (SELECT * FROM orders)"),
            "{}",
            o[0].label
        );
        assert!(o[0].label.ends_with('…'));
        let kids: Vec<_> = o[0]
            .children
            .iter()
            .map(|c| (c.item, c.label.as_str()))
            .collect();
        assert_eq!(
            kids,
            vec![
                (OutlineKind::Cte, "x"),
                (OutlineKind::Table, "users u"),
                (OutlineKind::Table, "x"),
                (OutlineKind::Subquery, "(subquery)")
            ]
        );
        assert_eq!(o[0].children[0].children[0].label, "orders");
        assert_eq!(o[0].children[3].children[0].label, "sales.customers");
        assert_eq!(o[1].item, OutlineKind::ClientCommand);
        assert_eq!(o[2].kind, StatementKind::Insert);
        assert_eq!(o[2].children[0].label, "logs");
        for item in &o {
            assert!(
                src.is_char_boundary(item.start)
                    && src.is_char_boundary(item.end)
                    && item.start <= item.end
            );
        }
        assert_eq!(&src[o[1].start..o[1].end], "@set a = 1");
    }

    #[test]
    fn derived_and_unions() {
        let o = outline(
            "SELECT * FROM (SELECT a FROM t1) s UNION SELECT b FROM t2",
            &pg(),
        );
        let labels: Vec<_> = o[0].children.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, vec!["(subquery) s", "t2"]);
        assert_eq!(o[0].children[0].children[0].label, "t1");
    }

    #[test]
    fn random_inputs_never_panic() {
        let mut rng = Rng::new(41);
        for _ in 0..300 {
            let src = random_sql(&mut rng, 30);
            for syn in all_syntaxes() {
                for item in outline(&src, &syn) {
                    assert!(item.start <= item.end && item.end <= src.len());
                }
            }
        }
    }
}
