//! Execution plan model produced by `Session::explain` for every database.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PlanNode {
    /// Operation, e.g. "Seq Scan", "Hash Join", "ALL", "SEARCH".
    pub node_type: String,
    /// Relation / index the node works on.
    pub relation: Option<String>,
    pub alias: Option<String>,
    pub index: Option<String>,
    /// Filter / join condition text.
    pub condition: Option<String>,
    pub startup_cost: Option<f64>,
    pub total_cost: Option<f64>,
    pub plan_rows: Option<f64>,
    pub plan_width: Option<f64>,
    /// Present when the plan was produced by EXPLAIN ANALYZE.
    pub actual_rows: Option<f64>,
    pub actual_total_ms: Option<f64>,
    pub actual_startup_ms: Option<f64>,
    pub loops: Option<f64>,
    /// Every other property reported by the database, in server order.
    pub properties: Vec<(String, String)>,
    pub children: Vec<PlanNode>,
}

impl PlanNode {
    pub fn new(node_type: impl Into<String>) -> Self {
        PlanNode { node_type: node_type.into(), ..Default::default() }
    }

    /// Number of nodes in this subtree.
    pub fn count(&self) -> usize {
        1 + self.children.iter().map(PlanNode::count).sum::<usize>()
    }

    /// Time spent in this node excluding children (EXPLAIN ANALYZE only).
    pub fn exclusive_ms(&self) -> Option<f64> {
        let total = self.actual_total_ms? * self.loops.unwrap_or(1.0);
        let children: f64 = self
            .children
            .iter()
            .filter_map(|c| c.actual_total_ms.map(|t| t * c.loops.unwrap_or(1.0)))
            .sum();
        Some((total - children).max(0.0))
    }

    /// Cost excluding children.
    pub fn exclusive_cost(&self) -> Option<f64> {
        let total = self.total_cost?;
        let children: f64 = self.children.iter().filter_map(|c| c.total_cost).sum();
        Some((total - children).max(0.0))
    }

    /// Short title for diagram boxes: "Index Scan on users (users_pkey)".
    pub fn title(&self) -> String {
        let mut s = self.node_type.clone();
        if let Some(r) = &self.relation {
            s.push_str(" on ");
            s.push_str(r);
            if let Some(a) = &self.alias {
                if a != r {
                    s.push(' ');
                    s.push_str(a);
                }
            }
        }
        if let Some(i) = &self.index {
            s.push_str(&format!(" ({i})"));
        }
        s
    }

    /// Depth-first visit with depth.
    pub fn walk<'a>(&'a self, depth: usize, f: &mut dyn FnMut(&'a PlanNode, usize)) {
        f(self, depth);
        for c in &self.children {
            c.walk(depth + 1, f);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QueryPlan {
    /// Root nodes (usually one).
    pub roots: Vec<PlanNode>,
    /// Text form as printed by the database (for the "text" plan view).
    pub text: String,
    /// Raw machine readable output (JSON) when available.
    pub raw: Option<String>,
    pub planning_ms: Option<f64>,
    pub execution_ms: Option<f64>,
    pub analyzed: bool,
}

impl QueryPlan {
    /// Render the node tree as indented text (used when the server gives no text form).
    pub fn to_text_tree(&self) -> String {
        let mut out = String::new();
        for r in &self.roots {
            r.walk(0, &mut |n, depth| {
                let indent = "  ".repeat(depth);
                let arrow = if depth > 0 { "->  " } else { "" };
                out.push_str(&format!("{indent}{arrow}{}", n.title()));
                if let (Some(s), Some(t)) = (n.startup_cost, n.total_cost) {
                    out.push_str(&format!("  (cost={s:.2}..{t:.2}"));
                    if let Some(r) = n.plan_rows {
                        out.push_str(&format!(" rows={r:.0}"));
                    }
                    out.push(')');
                } else if let Some(r) = n.plan_rows {
                    out.push_str(&format!("  (rows={r:.0})"));
                }
                if let (Some(t), Some(r)) = (n.actual_total_ms, n.actual_rows) {
                    out.push_str(&format!(" (actual time={t:.3} rows={r:.0} loops={})", n.loops.unwrap_or(1.0)));
                }
                out.push('\n');
                if let Some(c) = &n.condition {
                    out.push_str(&format!("{indent}      {c}\n"));
                }
            });
        }
        out
    }
}
