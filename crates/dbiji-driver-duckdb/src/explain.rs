//! `EXPLAIN` / `EXPLAIN ANALYZE` through DuckDB's JSON plan output.
//!
//! * `EXPLAIN (FORMAT json)` gives the physical plan as
//!   `[{"name", "extra_info": {...}, "children": [...]}]`; the text form of
//!   the plan comes from a plain `EXPLAIN`.
//! * `EXPLAIN (ANALYZE, FORMAT json)` runs the statement and returns the query
//!   profile: `{"latency", ..., "children": [{"operator_name",
//!   "operator_timing", "operator_cardinality", "extra_info", "children"}]}`.
//!   Operator timings are exclusive; [`PlanNode::actual_total_ms`] holds the
//!   inclusive time (own time plus children).

use crate::lexer;
use crate::worker::Worker;
use dbiji_core::dialect::ansi::strip_trailing_semicolon;
use dbiji_core::plan::{PlanNode, QueryPlan};
use dbiji_core::{DbError, DbResult};
use serde_json::Value as J;

fn explain_rows(w: &Worker, sql: &str) -> DbResult<Vec<(String, String)>> {
    w.query_map(sql, &[], |r| {
        (
            r.get::<_, Option<String>>(0)
                .ok()
                .flatten()
                .unwrap_or_default(),
            r.get::<_, Option<String>>(1)
                .ok()
                .flatten()
                .unwrap_or_default(),
        )
    })
}

/// Value of the row named `key` (or the first row).
fn pick(rows: Vec<(String, String)>, key: &str) -> String {
    let mut first = None;
    for (k, v) in rows {
        if k == key {
            return v;
        }
        first.get_or_insert(v);
    }
    first.unwrap_or_default()
}

fn json_text(v: &J) -> String {
    match v {
        J::String(s) => s.trim().to_string(),
        J::Array(items) => items
            .iter()
            .map(json_text)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        J::Null => String::new(),
        other => other.to_string(),
    }
}

fn number(s: &str) -> Option<f64> {
    s.trim()
        .trim_start_matches('~')
        .replace(',', "")
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Copy `extra_info` into the node: well known keys become node fields.
fn apply_extra(node: &mut PlanNode, extra: &J) {
    let Some(map) = extra.as_object() else { return };
    let mut conditions = Vec::new();
    for (key, value) in map {
        let text = json_text(value);
        if text.is_empty() {
            continue;
        }
        match key.as_str() {
            "Table" | "Table Name" => node.relation = Some(text),
            "Estimated Cardinality" | "__estimated_cardinality__" => node.plan_rows = number(&text),
            "__cardinality__" => {
                if node.actual_rows.is_none() {
                    node.actual_rows = number(&text);
                }
            }
            "__timing__" => {}
            "Filters" | "Conditions" | "Join Condition" => {
                conditions.push(text.replace('\n', " AND "))
            }
            _ => node
                .properties
                .push((key.clone(), text.replace('\n', ", "))),
        }
    }
    if !conditions.is_empty() {
        node.condition = Some(conditions.join(" AND "));
    }
}

fn children(v: &J, f: fn(&J) -> Vec<PlanNode>) -> Vec<PlanNode> {
    v.get("children")
        .and_then(J::as_array)
        .map(|c| c.iter().flat_map(f).collect())
        .unwrap_or_default()
}

/// Physical plan node of `EXPLAIN (FORMAT json)`.
fn plan_nodes(v: &J) -> Vec<PlanNode> {
    let name = v
        .get("name")
        .and_then(J::as_str)
        .unwrap_or("?")
        .trim()
        .to_string();
    let kids = children(v, plan_nodes);
    if name.eq_ignore_ascii_case("EXPLAIN_ANALYZE") {
        return kids;
    }
    let mut node = PlanNode::new(name);
    if let Some(extra) = v.get("extra_info") {
        apply_extra(&mut node, extra);
    }
    node.children = kids;
    vec![node]
}

/// Operator of an `EXPLAIN (ANALYZE, FORMAT json)` profile.
fn profile_nodes(v: &J) -> Vec<PlanNode> {
    let name = v
        .get("operator_name")
        .and_then(J::as_str)
        .unwrap_or("?")
        .trim()
        .to_string();
    let kids = children(v, profile_nodes);
    if name.eq_ignore_ascii_case("EXPLAIN_ANALYZE") {
        return kids;
    }
    let mut node = PlanNode::new(name);
    let own_ms = v
        .get("operator_timing")
        .and_then(J::as_f64)
        .map(|s| s * 1000.0);
    node.actual_rows = v.get("operator_cardinality").and_then(J::as_f64);
    let child_ms: f64 = kids.iter().filter_map(|c| c.actual_total_ms).sum();
    node.actual_total_ms = own_ms.map(|t| t + child_ms);
    node.loops = Some(1.0);
    if let Some(extra) = v.get("extra_info") {
        apply_extra(&mut node, extra);
    }
    if let Some(t) = own_ms {
        node.properties
            .push(("Operator time (ms)".into(), format!("{t:.3}")));
    }
    for key in [
        "operator_rows_scanned",
        "operator_row_groups_scanned",
        "result_set_size",
        "cpu_time",
    ] {
        if let Some(x) = v.get(key).filter(|x| x.is_number()) {
            node.properties.push((key.to_string(), x.to_string()));
        }
    }
    node.children = kids;
    vec![node]
}

fn parse_error(e: serde_json::Error) -> DbError {
    DbError::internal(format!("Cannot read the DuckDB plan: {e}"))
}

/// Explain `sql` (one statement).
pub(crate) fn explain(w: &mut Worker, sql: &str, analyze: bool) -> DbResult<QueryPlan> {
    let stmt = strip_trailing_semicolon(sql.trim()).trim();
    match lexer::split_statements(stmt).len() {
        0 => return Err(DbError::query("There is no statement to explain")),
        1 => {}
        _ => return Err(DbError::query("EXPLAIN works on a single statement")),
    }
    let mut plan = QueryPlan {
        analyzed: analyze,
        ..Default::default()
    };
    if analyze {
        // the statement really runs: keep it inside the manual transaction
        w.implicit_begin()?;
        let raw = pick(
            explain_rows(w, &format!("EXPLAIN (ANALYZE, FORMAT json) {stmt}"))?,
            "analyzed_plan",
        );
        let root: J = serde_json::from_str(&raw).map_err(parse_error)?;
        plan.roots = children(&root, profile_nodes);
        plan.execution_ms = root.get("latency").and_then(J::as_f64).map(|s| s * 1000.0);
        plan.text = plan.to_text_tree();
        plan.raw = Some(raw);
    } else {
        let raw = pick(
            explain_rows(w, &format!("EXPLAIN (FORMAT json) {stmt}"))?,
            "physical_plan",
        );
        let root: J = serde_json::from_str(&raw).map_err(parse_error)?;
        plan.roots = match &root {
            J::Array(items) => items.iter().flat_map(plan_nodes).collect(),
            other => plan_nodes(other),
        };
        plan.text = pick(
            explain_rows(w, &format!("EXPLAIN {stmt}"))?,
            "physical_plan",
        );
        plan.raw = Some(raw);
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_plan_json() {
        let raw = r#"[{"name": "PROJECTION", "children": [{"name": "SEQ_SCAN ", "children": [],
            "extra_info": {"Table": "t", "Type": "Sequential Scan", "Filters": ["a>1", "b<2"],
            "__estimated_cardinality__": "3"}}], "extra_info": {"Projections": "a"}}]"#;
        let root: J = serde_json::from_str(raw).unwrap();
        let nodes: Vec<PlanNode> = root
            .as_array()
            .unwrap()
            .iter()
            .flat_map(plan_nodes)
            .collect();
        assert_eq!(nodes.len(), 1);
        let scan = &nodes[0].children[0];
        assert_eq!(scan.node_type, "SEQ_SCAN");
        assert_eq!(scan.relation.as_deref(), Some("t"));
        assert_eq!(scan.plan_rows, Some(3.0));
        assert_eq!(scan.condition.as_deref(), Some("a>1 AND b<2"));
        assert_eq!(
            nodes[0].properties,
            vec![("Projections".to_string(), "a".to_string())]
        );
    }

    #[test]
    fn profile_json() {
        let raw = r#"{"latency": 0.5, "children": [{"operator_name": "EXPLAIN_ANALYZE", "children": [
            {"operator_name": "PROJECTION", "operator_timing": 0.001, "operator_cardinality": 10,
             "extra_info": {}, "children": [{"operator_name": "TABLE_SCAN", "operator_timing": 0.002,
             "operator_cardinality": 10, "extra_info": {"Table": "t"}, "children": []}]}]}]}"#;
        let root: J = serde_json::from_str(raw).unwrap();
        let roots = children(&root, profile_nodes);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].node_type, "PROJECTION");
        assert!((roots[0].actual_total_ms.unwrap() - 3.0).abs() < 1e-9);
        assert!((roots[0].exclusive_ms().unwrap() - 1.0).abs() < 1e-9);
        assert_eq!(roots[0].children[0].relation.as_deref(), Some("t"));
    }
}
