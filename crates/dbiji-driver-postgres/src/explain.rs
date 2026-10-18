//! `EXPLAIN` support: `FORMAT JSON` parsed into [`QueryPlan`] / [`PlanNode`]
//! for PostgreSQL, the text tree of CockroachDB parsed best-effort.

use crate::error::map_error;
use crate::session::PgSession;
use crate::sqltext::{ScriptInfo, strip_semicolon};
use dbiji_core::plan::{PlanNode, QueryPlan};
use dbiji_core::{DbError, DbResult};
use serde_json::Value as J;

const CONDITION_KEYS: &[&str] = &["Index Cond", "Hash Cond", "Merge Cond", "Join Filter", "Recheck Cond", "Filter"];

fn json_text(v: &J) -> String {
    match v {
        J::String(s) => s.clone(),
        J::Number(n) => n.to_string(),
        J::Bool(b) => b.to_string(),
        J::Null => String::new(),
        J::Array(items) if items.iter().all(|i| i.is_string() || i.is_number()) => items.iter().map(json_text).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

fn parse_node(v: &J) -> PlanNode {
    let mut n = PlanNode::default();
    let Some(obj) = v.as_object() else { return n };
    let mut conditions: Vec<(usize, String)> = Vec::new();
    for (k, val) in obj {
        match k.as_str() {
            "Node Type" => n.node_type = json_text(val),
            "Relation Name" => n.relation = Some(json_text(val)),
            "Alias" => n.alias = Some(json_text(val)),
            "Index Name" => n.index = Some(json_text(val)),
            "Startup Cost" => n.startup_cost = val.as_f64(),
            "Total Cost" => n.total_cost = val.as_f64(),
            "Plan Rows" => n.plan_rows = val.as_f64(),
            "Plan Width" => n.plan_width = val.as_f64(),
            "Actual Startup Time" => n.actual_startup_ms = val.as_f64(),
            "Actual Total Time" => n.actual_total_ms = val.as_f64(),
            "Actual Rows" => n.actual_rows = val.as_f64(),
            "Actual Loops" => n.loops = val.as_f64(),
            "Plans" => {
                if let Some(children) = val.as_array() {
                    n.children = children.iter().map(parse_node).collect();
                }
            }
            key if CONDITION_KEYS.contains(&key) => {
                let order = CONDITION_KEYS.iter().position(|c| *c == key).unwrap_or(0);
                conditions.push((order, format!("{key}: {}", json_text(val))));
                n.properties.push((k.clone(), json_text(val)));
            }
            _ => n.properties.push((k.clone(), json_text(val))),
        }
    }
    if n.relation.is_none() {
        for key in ["CTE Name", "Function Name", "Subplan Name"] {
            if let Some(v) = obj.get(key) {
                n.relation = Some(json_text(v));
                break;
            }
        }
    }
    if !conditions.is_empty() {
        conditions.sort_by_key(|(o, _)| *o);
        n.condition = Some(conditions.into_iter().map(|(_, c)| c).collect::<Vec<_>>().join("; "));
    }
    n
}

/// Parse the output of `EXPLAIN (FORMAT JSON ...)`.
pub(crate) fn parse_json_plan(json: &str, analyzed: bool) -> DbResult<QueryPlan> {
    let v: J = serde_json::from_str(json).map_err(|e| DbError::internal(format!("Cannot parse the JSON plan: {e}")))?;
    let items: Vec<&J> = match &v {
        J::Array(a) => a.iter().collect(),
        other => vec![other],
    };
    let mut plan = QueryPlan { raw: Some(json.to_string()), analyzed, ..Default::default() };
    for item in items {
        let Some(obj) = item.as_object() else { continue };
        let mut root = obj.get("Plan").map(parse_node).unwrap_or_default();
        for (k, val) in obj {
            match k.as_str() {
                "Plan" => {}
                "Planning Time" => plan.planning_ms = val.as_f64(),
                "Execution Time" => plan.execution_ms = val.as_f64(),
                "Triggers" => {
                    for t in val.as_array().into_iter().flatten() {
                        let name = t.get("Trigger Name").map(json_text).unwrap_or_default();
                        let rel = t.get("Relation").map(json_text).unwrap_or_default();
                        let time = t.get("Time").and_then(J::as_f64).unwrap_or(0.0);
                        let calls = t.get("Calls").map(json_text).unwrap_or_default();
                        root.properties.push((format!("Trigger {name}"), format!("on {rel}: time={time:.3} calls={calls}")));
                    }
                }
                "Planning" => {
                    if let Some(p) = val.as_object() {
                        for (pk, pv) in p {
                            root.properties.push((format!("Planning {pk}"), json_text(pv)));
                        }
                    }
                }
                _ => root.properties.push((k.clone(), json_text(val))),
            }
        }
        plan.roots.push(root);
    }
    Ok(plan)
}

/// Best-effort parser for CockroachDB's tree output (`• node`, `key: value` lines).
pub(crate) fn parse_cockroach_plan(lines: &[String], analyzed: bool) -> QueryPlan {
    let mut plan = QueryPlan { text: lines.join("\n"), analyzed, ..Default::default() };
    // (indent, node)
    let mut stack: Vec<(usize, PlanNode)> = Vec::new();
    let mut header: Vec<(String, String)> = Vec::new();
    fn attach(stack: &mut Vec<(usize, PlanNode)>, roots: &mut Vec<PlanNode>, min_indent: usize) {
        while let Some((ind, _)) = stack.last() {
            if *ind < min_indent {
                break;
            }
            let Some((_, node)) = stack.pop() else { break };
            match stack.last_mut() {
                Some((_, parent)) => parent.children.push(node),
                None => roots.push(node),
            }
        }
    }
    for line in lines {
        if let Some(pos) = line.find('•') {
            let indent = line[..pos].chars().count();
            attach(&mut stack, &mut plan.roots, indent);
            let name = line[pos + '•'.len_utf8()..].trim().to_string();
            stack.push((indent, PlanNode::new(name)));
            continue;
        }
        let cleaned: String = line.chars().skip_while(|c| matches!(c, '│' | '├' | '└' | '─' | ' ')).collect();
        let Some((k, v)) = cleaned.split_once(':') else { continue };
        let (k, v) = (k.trim().to_string(), v.trim().to_string());
        match stack.last_mut() {
            Some((_, node)) => {
                match k.as_str() {
                    "table" => {
                        let (t, i) = v.split_once('@').map(|(a, b)| (a.to_string(), Some(b.to_string()))).unwrap_or((v.clone(), None));
                        node.relation = Some(t);
                        node.index = i;
                    }
                    "estimated row count" => {
                        node.plan_rows = v.split_whitespace().next().and_then(|n| n.replace(',', "").parse().ok());
                    }
                    "actual row count" => {
                        node.actual_rows = v.split_whitespace().next().and_then(|n| n.replace(',', "").parse().ok());
                    }
                    "filter" | "spans" | "equality" | "on" => {
                        node.condition = Some(match node.condition.take() {
                            Some(c) => format!("{c}; {k}: {v}"),
                            None => format!("{k}: {v}"),
                        });
                    }
                    _ => {}
                }
                node.properties.push((k, v));
            }
            None => {
                if k == "planning time" {
                    plan.planning_ms = parse_ms(&v);
                } else if k == "execution time" {
                    plan.execution_ms = parse_ms(&v);
                }
                header.push((k, v));
            }
        }
    }
    attach(&mut stack, &mut plan.roots, 0);
    if let Some(root) = plan.roots.first_mut() {
        for (k, v) in header.into_iter().rev() {
            root.properties.insert(0, (k, v));
        }
    }
    plan
}

fn parse_ms(v: &str) -> Option<f64> {
    let t = v.trim();
    let num: String = t.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let n: f64 = num.parse().ok()?;
    let unit = t[num.len()..].trim();
    Some(match unit {
        "µs" | "us" => n / 1000.0,
        "s" => n * 1000.0,
        _ => n,
    })
}

impl PgSession {
    /// Run EXPLAIN on `sql`; ANALYZE runs inside a rolled back transaction (or savepoint).
    pub(crate) async fn explain_impl(&mut self, sql: &str, analyze: bool) -> DbResult<QueryPlan> {
        let stmt = strip_semicolon(sql.trim()).to_string();
        let script = ScriptInfo::analyze(&stmt);
        if script.is_empty() {
            return Err(DbError::query("Nothing to explain"));
        }
        if script.statements.len() > 1 {
            return Err(DbError::query("EXPLAIN works on a single statement"));
        }
        let has_params = script.statements[0].has_params;
        if self.is_cockroach() {
            let q = format!("EXPLAIN {}{stmt}", if analyze { "ANALYZE " } else { "" });
            let lines = self.explain_wrapped(&q, analyze, q.len() - stmt.len()).await?;
            return Ok(parse_cockroach_plan(&lines, analyze));
        }
        let generic = has_params && !analyze && self.pg_major() >= 16;
        let mut opts = vec!["FORMAT JSON"];
        if analyze {
            opts.push("ANALYZE");
            opts.push("BUFFERS");
        }
        if generic {
            opts.push("GENERIC_PLAN");
        }
        let prefix = format!("EXPLAIN ({}) ", opts.join(", "));
        let q = format!("{prefix}{stmt}");
        let lines = self.explain_wrapped(&q, analyze, prefix.chars().count()).await?;
        let json = lines.join("\n");
        let mut plan = parse_json_plan(&json, analyze)?;
        if analyze {
            let mut text = plan.to_text_tree();
            if let Some(p) = plan.planning_ms {
                text.push_str(&format!("Planning Time: {p:.3} ms\n"));
            }
            if let Some(e) = plan.execution_ms {
                text.push_str(&format!("Execution Time: {e:.3} ms\n"));
            }
            plan.text = text;
        } else {
            let text_prefix = if generic { "EXPLAIN (FORMAT TEXT, GENERIC_PLAN) " } else { "EXPLAIN (FORMAT TEXT) " };
            let tq = format!("{text_prefix}{stmt}");
            plan.text = match self.explain_wrapped(&tq, false, text_prefix.len()).await {
                Ok(lines) => lines.join("\n"),
                Err(_) => plan.to_text_tree(),
            };
        }
        Ok(plan)
    }

    /// Run an EXPLAIN statement through the simple protocol and return its text lines.
    async fn explain_wrapped(&mut self, q: &str, analyze: bool, prefix_chars: usize) -> DbResult<Vec<String>> {
        let in_tx = self.shared.in_tx();
        if analyze {
            self.batch(if in_tx { "SAVEPOINT dbiji_explain" } else { "BEGIN" }).await?;
        }
        let res = self.client()?.simple_query(q).await;
        if analyze {
            let undo = if in_tx { "ROLLBACK TO SAVEPOINT dbiji_explain; RELEASE SAVEPOINT dbiji_explain" } else { "ROLLBACK" };
            if let Err(e) = self.batch(undo).await {
                tracing::warn!("rolling back EXPLAIN ANALYZE failed: {e}");
            }
        }
        let msgs = res.map_err(|e| {
            let mut err = map_error(&e);
            if let Some(p) = err.position {
                err.position = Some(p.saturating_sub(prefix_chars).max(1));
            }
            err
        })?;
        let mut lines = Vec::new();
        for m in msgs {
            if let tokio_postgres::SimpleQueryMessage::Row(r) = m {
                lines.push(r.get(0).unwrap_or_default().to_string());
            }
        }
        Ok(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"[
      {
        "Plan": {
          "Node Type": "Hash Join",
          "Parallel Aware": false,
          "Join Type": "Inner",
          "Startup Cost": 1.07,
          "Total Cost": 2.21,
          "Plan Rows": 5,
          "Plan Width": 72,
          "Actual Startup Time": 0.031,
          "Actual Total Time": 0.04,
          "Actual Rows": 5,
          "Actual Loops": 1,
          "Inner Unique": true,
          "Hash Cond": "(o.customer_id = c.id)",
          "Join Filter": "(o.total > c.limit)",
          "Plans": [
            {"Node Type": "Seq Scan", "Parent Relationship": "Outer", "Relation Name": "orders", "Alias": "o",
             "Startup Cost": 0.0, "Total Cost": 1.05, "Plan Rows": 5, "Plan Width": 40, "Filter": "(total > 0)",
             "Rows Removed by Filter": 2},
            {"Node Type": "Hash", "Parent Relationship": "Inner", "Startup Cost": 1.03, "Total Cost": 1.03,
             "Plan Rows": 3, "Plan Width": 36,
             "Plans": [{"Node Type": "Index Scan", "Relation Name": "customers", "Alias": "c", "Index Name": "customers_pkey",
                        "Index Cond": "(id > 0)", "Startup Cost": 0.1, "Total Cost": 1.02, "Plan Rows": 3, "Plan Width": 36}]}
          ]
        },
        "Planning Time": 0.125,
        "Triggers": [{"Trigger Name": "audit", "Relation": "orders", "Time": 0.05, "Calls": 5}],
        "Execution Time": 0.09
      }
    ]"#;

    #[test]
    fn json_plan() {
        let p = parse_json_plan(SAMPLE, true).unwrap();
        assert_eq!(p.roots.len(), 1);
        let r = &p.roots[0];
        assert_eq!(r.node_type, "Hash Join");
        assert_eq!(r.total_cost, Some(2.21));
        assert_eq!(r.actual_rows, Some(5.0));
        assert_eq!(r.loops, Some(1.0));
        assert_eq!(r.condition.as_deref(), Some("Hash Cond: (o.customer_id = c.id); Join Filter: (o.total > c.limit)"));
        assert!(r.properties.iter().any(|(k, v)| k == "Join Type" && v == "Inner"));
        assert!(r.properties.iter().any(|(k, _)| k == "Trigger audit"));
        assert_eq!(r.count(), 4);
        let scan = &r.children[0];
        assert_eq!(scan.relation.as_deref(), Some("orders"));
        assert_eq!(scan.alias.as_deref(), Some("o"));
        assert_eq!(scan.condition.as_deref(), Some("Filter: (total > 0)"));
        let idx = &r.children[1].children[0];
        assert_eq!(idx.index.as_deref(), Some("customers_pkey"));
        assert_eq!(idx.title(), "Index Scan on customers c (customers_pkey)");
        assert_eq!(p.planning_ms, Some(0.125));
        assert_eq!(p.execution_ms, Some(0.09));
        assert!(p.analyzed);
        let text = p.to_text_tree();
        assert!(text.contains("->  Seq Scan on orders o"), "{text}");
    }

    #[test]
    fn cockroach_tree() {
        let lines: Vec<String> = [
            "distribution: local",
            "vectorized: true",
            "",
            "• filter",
            "│ estimated row count: 1",
            "│ filter: a > 1",
            "│",
            "└── • scan",
            "      estimated row count: 3 (100% of the table)",
            "      table: t@t_pkey",
            "      spans: FULL SCAN",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let p = parse_cockroach_plan(&lines, false);
        assert_eq!(p.roots.len(), 1);
        assert_eq!(p.roots[0].node_type, "filter");
        assert_eq!(p.roots[0].plan_rows, Some(1.0));
        assert_eq!(p.roots[0].children.len(), 1);
        let scan = &p.roots[0].children[0];
        assert_eq!(scan.relation.as_deref(), Some("t"));
        assert_eq!(scan.index.as_deref(), Some("t_pkey"));
        assert_eq!(scan.plan_rows, Some(3.0));
        assert!(p.roots[0].properties.iter().any(|(k, v)| k == "distribution" && v == "local"));
        assert_eq!(parse_ms("1.5ms"), Some(1.5));
        assert_eq!(parse_ms("250µs"), Some(0.25));
    }
}
