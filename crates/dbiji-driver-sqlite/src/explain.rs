//! `EXPLAIN QUERY PLAN` → [`QueryPlan`].

use crate::error::{ErrContext, map_error_ctx};
use crate::lexer::{self, TokKind};
use crate::worker::Worker;
use dbiji_core::dialect::ansi;
use dbiji_core::plan::{PlanNode, QueryPlan};
use dbiji_core::{DbError, DbResult};
use std::collections::HashMap;

/// One row of `EXPLAIN QUERY PLAN`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlanRow {
    pub id: i64,
    pub parent: i64,
    pub detail: String,
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len()
        && s.is_char_boundary(prefix.len())
        && s[..prefix.len()].eq_ignore_ascii_case(prefix)
    {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

/// Split a trailing `( ... )` group off `s`.
fn trailing_group(s: &str) -> (&str, Option<&str>) {
    let t = s.trim_end();
    if !t.ends_with(')') {
        return (t, None);
    }
    let mut depth = 0i32;
    for (i, c) in t.char_indices().rev() {
        match c {
            ')' => depth += 1,
            '(' => {
                depth -= 1;
                if depth == 0 {
                    return (t[..i].trim_end(), Some(&t[i + 1..t.len() - 1]));
                }
            }
            _ => {}
        }
    }
    (t, None)
}

/// Structure the `detail` text of one plan row.
pub(crate) fn parse_detail(detail: &str) -> PlanNode {
    let detail = detail.trim();
    let mut node = PlanNode::new(detail);
    node.properties.push(("Detail".into(), detail.to_string()));
    let scan = strip_prefix_ci(detail, "SCAN ").map(|r| ("SCAN", r));
    let search = strip_prefix_ci(detail, "SEARCH ").map(|r| ("SEARCH", r));
    if let Some((kind, rest)) = scan.or(search) {
        if strip_prefix_ci(rest, "CONSTANT ROW").is_some() {
            node.node_type = "SCAN CONSTANT ROW".into();
            return node;
        }
        node.node_type = kind.into();
        let (body, cond) = trailing_group(rest);
        node.condition = cond.map(str::to_string);
        let (relation, using) = match body.find(" USING ") {
            Some(p) => (&body[..p], Some(&body[p + 7..])),
            None => match body.find(" VIRTUAL TABLE ") {
                Some(p) => (&body[..p], Some(&body[p + 1..])),
                None => (body, None),
            },
        };
        let relation = relation.trim();
        if let Some((r, alias)) = relation.split_once(" AS ") {
            node.relation = Some(r.trim().to_string());
            node.alias = Some(alias.trim().to_string());
        } else if !relation.is_empty() {
            node.relation = Some(relation.to_string());
        }
        if let Some(u) = using.map(str::trim) {
            node.properties.push(("Using".into(), u.to_string()));
            if let Some(name) = strip_prefix_ci(u, "COVERING INDEX ") {
                node.index = Some(name.trim().to_string());
                node.properties.push(("Covering".into(), "yes".into()));
            } else if let Some(name) = strip_prefix_ci(u, "INDEX ") {
                node.index = Some(name.trim().to_string());
            } else if !u.is_empty() {
                node.index = Some(u.to_string());
                if u.to_ascii_uppercase().contains("COVERING") {
                    node.properties.push(("Covering".into(), "yes".into()));
                }
            }
        }
        return node;
    }
    if let Some(rest) = strip_prefix_ci(detail, "USE TEMP B-TREE FOR ") {
        node.node_type = "USE TEMP B-TREE".into();
        node.properties
            .push(("Purpose".into(), rest.trim().to_string()));
        return node;
    }
    for kind in ["CO-ROUTINE ", "MATERIALIZE "] {
        if let Some(rest) = strip_prefix_ci(detail, kind) {
            node.node_type = kind.trim().into();
            node.relation = Some(rest.trim().to_string());
            return node;
        }
    }
    if let Some(rest) = strip_prefix_ci(detail, "BLOOM FILTER ON ") {
        node.node_type = "BLOOM FILTER".into();
        let (body, cond) = trailing_group(rest);
        node.relation = Some(body.trim().to_string());
        node.condition = cond.map(str::to_string);
        return node;
    }
    node
}

/// Build the node tree from plan rows (`parent` refers to `id`, 0 = root).
pub(crate) fn build_tree(rows: &[PlanRow]) -> Vec<PlanNode> {
    let ids: HashMap<i64, usize> = rows.iter().enumerate().map(|(i, r)| (r.id, i)).collect();
    let mut children: HashMap<i64, Vec<usize>> = HashMap::new();
    let mut roots = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        if r.parent != r.id && ids.contains_key(&r.parent) {
            children.entry(r.parent).or_default().push(i);
        } else {
            roots.push(i);
        }
    }
    fn build(
        i: usize,
        rows: &[PlanRow],
        children: &HashMap<i64, Vec<usize>>,
        depth: usize,
    ) -> PlanNode {
        let mut node = parse_detail(&rows[i].detail);
        if depth < 256
            && let Some(kids) = children.get(&rows[i].id)
        {
            for &k in kids {
                node.children.push(build(k, rows, children, depth + 1));
            }
        }
        node
    }
    roots
        .into_iter()
        .map(|i| build(i, rows, &children, 0))
        .collect()
}

/// Text form as printed by the `sqlite3` shell.
pub(crate) fn render_text(rows: &[PlanRow]) -> String {
    fn walk(rows: &[PlanRow], parent: i64, prefix: &str, out: &mut String, depth: usize) {
        let kids: Vec<&PlanRow> = rows
            .iter()
            .filter(|r| r.parent == parent && r.id != parent)
            .collect();
        for (n, r) in kids.iter().enumerate() {
            let last = n + 1 == kids.len();
            out.push_str(prefix);
            out.push_str(if last { "`--" } else { "|--" });
            out.push_str(&r.detail);
            out.push('\n');
            if depth < 256 {
                let next = format!("{prefix}{}", if last { "   " } else { "|  " });
                walk(rows, r.id, &next, out, depth + 1);
            }
        }
    }
    let mut out = String::from("QUERY PLAN\n");
    let ids: std::collections::HashSet<i64> = rows.iter().map(|r| r.id).collect();
    // rows whose parent is unknown are shown at the top level
    let mut normalized: Vec<PlanRow> = rows.to_vec();
    for r in &mut normalized {
        if r.parent != 0 && !ids.contains(&r.parent) {
            r.parent = 0;
        }
    }
    walk(&normalized, 0, "", &mut out, 0);
    out
}

/// Skip leading `EXPLAIN [QUERY PLAN]` keywords of `stmt`; returns the byte
/// offset where the statement to explain starts.
fn body_start(stmt: &str) -> usize {
    let toks = lexer::significant(stmt);
    let mut i = 0;
    if toks.first().is_some_and(|t| t.is_word("EXPLAIN")) {
        i = 1;
        if toks.get(1).is_some_and(|t| t.is_word("QUERY"))
            && toks.get(2).is_some_and(|t| t.is_word("PLAN"))
        {
            i = 3;
        }
    }
    toks.get(i)
        .filter(|t| t.kind != TokKind::Semi)
        .map(|t| t.start)
        .unwrap_or(stmt.len())
}

pub(crate) fn explain(w: &Worker, sql: &str) -> DbResult<QueryPlan> {
    let statements = lexer::split_statements(sql);
    let first = statements
        .first()
        .ok_or_else(|| DbError::query("There is no statement to explain"))?;
    let start = body_start(first.text);
    let body = ansi::strip_trailing_semicolon(&first.text[start..]);
    if body.trim().is_empty() {
        return Err(DbError::query("There is no statement to explain"));
    }
    const PREFIX: &str = "EXPLAIN QUERY PLAN ";
    let q = format!("{PREFIX}{body}");
    let map = |e: rusqlite::Error| {
        let ctx = ErrContext {
            sql: Some(&q),
            ..w.err_ctx(None, 0)
        };
        let mut err = map_error_ctx(e, &ctx);
        if let Some(p) = err.position {
            let abs = first.offset + start;
            let before = sql.get(..abs).map(|s| s.chars().count()).unwrap_or(0);
            err.position = Some(before + p.saturating_sub(PREFIX.len()).max(1));
        }
        err
    };
    let mut stmt = w.conn.prepare(&q).map_err(map)?;
    let mut rows = stmt.raw_query();
    let mut plan_rows = Vec::new();
    while let Some(r) = rows.next().map_err(map)? {
        let id = r.get::<_, i64>(0).unwrap_or(0);
        let parent = r.get::<_, i64>(1).unwrap_or(0);
        let detail = match r.get_ref(3) {
            Ok(v) => match v {
                rusqlite::types::ValueRef::Text(b) => String::from_utf8_lossy(b).into_owned(),
                _ => String::new(),
            },
            Err(_) => String::new(),
        };
        plan_rows.push(PlanRow { id, parent, detail });
    }
    let raw = serde_json::Value::Array(
        plan_rows
            .iter()
            .map(|r| serde_json::json!({ "id": r.id, "parent": r.parent, "detail": r.detail }))
            .collect(),
    );
    Ok(QueryPlan {
        roots: build_tree(&plan_rows),
        text: render_text(&plan_rows),
        raw: Some(raw.to_string()),
        planning_ms: None,
        execution_ms: None,
        analyzed: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn details() {
        let n = parse_detail("SEARCH users AS u USING COVERING INDEX ix_email (email=? AND id>?)");
        assert_eq!(n.node_type, "SEARCH");
        assert_eq!(n.relation.as_deref(), Some("users"));
        assert_eq!(n.alias.as_deref(), Some("u"));
        assert_eq!(n.index.as_deref(), Some("ix_email"));
        assert!(
            n.properties
                .iter()
                .any(|(k, v)| k == "Covering" && v == "yes")
        );
        assert_eq!(n.condition.as_deref(), Some("email=? AND id>?"));
        let n = parse_detail("SEARCH p USING INTEGER PRIMARY KEY (rowid=?)");
        assert_eq!(n.index.as_deref(), Some("INTEGER PRIMARY KEY"));
        assert_eq!(n.condition.as_deref(), Some("rowid=?"));
        let n = parse_detail("SCAN t");
        assert_eq!(
            (n.node_type.as_str(), n.relation.as_deref()),
            ("SCAN", Some("t"))
        );
        assert_eq!(
            parse_detail("SCAN CONSTANT ROW").node_type,
            "SCAN CONSTANT ROW"
        );
        let n = parse_detail("USE TEMP B-TREE FOR ORDER BY");
        assert_eq!(n.node_type, "USE TEMP B-TREE");
        assert_eq!(
            parse_detail("CO-ROUTINE sub").relation.as_deref(),
            Some("sub")
        );
        assert_eq!(
            parse_detail("CORRELATED SCALAR SUBQUERY 1").node_type,
            "CORRELATED SCALAR SUBQUERY 1"
        );
        let n = parse_detail("SCAN f VIRTUAL TABLE INDEX 0:M1");
        assert_eq!(n.relation.as_deref(), Some("f"));
        assert_eq!(n.index.as_deref(), Some("VIRTUAL TABLE INDEX 0:M1"));
    }

    #[test]
    fn tree_and_text() {
        let rows = vec![
            PlanRow {
                id: 2,
                parent: 0,
                detail: "SCAN a".into(),
            },
            PlanRow {
                id: 5,
                parent: 0,
                detail: "SEARCH b USING INDEX ib (x=?)".into(),
            },
            PlanRow {
                id: 9,
                parent: 5,
                detail: "CORRELATED SCALAR SUBQUERY 1".into(),
            },
            PlanRow {
                id: 12,
                parent: 9,
                detail: "SCAN c".into(),
            },
        ];
        let roots = build_tree(&rows);
        assert_eq!(roots.len(), 2);
        assert_eq!(
            roots[1].children[0].children[0].relation.as_deref(),
            Some("c")
        );
        let text = render_text(&rows);
        assert_eq!(
            text,
            "QUERY PLAN\n|--SCAN a\n`--SEARCH b USING INDEX ib (x=?)\n   `--CORRELATED SCALAR SUBQUERY 1\n      `--SCAN c\n"
        );
    }

    #[test]
    fn explain_prefix() {
        assert_eq!(body_start("EXPLAIN QUERY PLAN SELECT 1"), 19);
        assert_eq!(body_start("explain select 1"), 8);
        assert_eq!(body_start("SELECT 1"), 0);
    }
}
