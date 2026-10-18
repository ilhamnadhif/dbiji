//! Plan analysis: flattened node list with exclusive time/cost shares, the
//! plan summary (hotspots) and warnings about common problems.

use dbiji_core::plan::{PlanNode, QueryPlan};
use dbiji_core::{tr, trf};
use serde::{Deserialize, Serialize};

/// Path of a node: index of the root in `QueryPlan::roots`, then the child
/// index at every level.
pub type PlanNodePath = Vec<usize>;

/// Which measure drives colours and percentages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PlanMetric {
    /// Actual time when the plan was analyzed, cost otherwise.
    #[default]
    Auto,
    /// Exclusive actual time (EXPLAIN ANALYZE).
    Time,
    /// Exclusive estimated cost.
    Cost,
}

impl PlanMetric {
    /// English label (route through `tr` for display).
    pub fn label(self) -> &'static str {
        match self {
            PlanMetric::Auto => "Auto",
            PlanMetric::Time => "Time",
            PlanMetric::Cost => "Cost",
        }
    }

    /// Resolves `Auto` for a plan.
    pub fn resolve(self, plan: &QueryPlan) -> PlanMetric {
        match self {
            PlanMetric::Auto => {
                if plan.analyzed || has_actuals(plan) {
                    PlanMetric::Time
                } else {
                    PlanMetric::Cost
                }
            }
            m => m,
        }
    }
}

fn has_actuals(plan: &QueryPlan) -> bool {
    plan.roots.iter().any(|r| r.actual_total_ms.is_some())
}

/// One node of the flattened plan (depth-first order).
#[derive(Debug, Clone)]
pub struct FlatPlanNode<'a> {
    /// Path of the node.
    pub path: PlanNodePath,
    /// Depth (roots are 0).
    pub depth: usize,
    /// The node itself.
    pub node: &'a PlanNode,
    /// Index of the parent in the flat list.
    pub parent: Option<usize>,
    /// Indices of the children in the flat list.
    pub children: Vec<usize>,
    /// Total time including children over all loops (ms).
    pub inclusive_ms: Option<f64>,
    /// Time spent in this node only (ms).
    pub exclusive_ms: Option<f64>,
    /// Cost of this node only.
    pub exclusive_cost: Option<f64>,
    /// `exclusive_ms` relative to the whole plan (0..=1).
    pub time_share: Option<f64>,
    /// `exclusive_cost` relative to the whole plan (0..=1).
    pub cost_share: Option<f64>,
    /// Actual rows / estimated rows (`None` without both numbers).
    pub row_factor: Option<f64>,
}

impl FlatPlanNode<'_> {
    /// Share for the given (resolved) metric.
    pub fn share(&self, metric: PlanMetric) -> Option<f64> {
        match metric {
            PlanMetric::Cost => self.cost_share,
            _ => self.time_share,
        }
    }

    /// Rows flowing out of the node: actual rows over all loops when
    /// analyzed, else the estimate.
    pub fn output_rows(&self) -> Option<f64> {
        match self.node.actual_rows {
            Some(r) => Some(r * self.node.loops.unwrap_or(1.0).max(1.0)),
            None => self.node.plan_rows,
        }
    }
}

/// Flattens the plan depth-first and computes exclusive measures and shares.
pub fn flatten_plan(plan: &QueryPlan) -> Vec<FlatPlanNode<'_>> {
    let mut out: Vec<FlatPlanNode<'_>> = Vec::new();
    fn visit<'a>(
        node: &'a PlanNode,
        path: PlanNodePath,
        parent: Option<usize>,
        out: &mut Vec<FlatPlanNode<'a>>,
    ) {
        let idx = out.len();
        let inclusive_ms = node
            .actual_total_ms
            .map(|t| t * node.loops.unwrap_or(1.0).max(0.0));
        let row_factor = match (node.actual_rows, node.plan_rows) {
            (Some(a), Some(p)) => Some(a.max(0.0).max(0.5) / p.max(0.0).max(0.5)),
            _ => None,
        };
        out.push(FlatPlanNode {
            depth: path.len().saturating_sub(1),
            path: path.clone(),
            node,
            parent,
            children: Vec::new(),
            inclusive_ms,
            exclusive_ms: node.exclusive_ms(),
            exclusive_cost: node.exclusive_cost(),
            time_share: None,
            cost_share: None,
            row_factor,
        });
        if let Some(p) = parent
            && let Some(pn) = out.get_mut(p)
        {
            pn.children.push(idx);
        }
        for (i, c) in node.children.iter().enumerate() {
            let mut child_path = path.clone();
            child_path.push(i);
            visit(c, child_path, Some(idx), out);
        }
    }
    for (i, r) in plan.roots.iter().enumerate() {
        visit(r, vec![i], None, &mut out);
    }
    let total_ms: f64 = out
        .iter()
        .filter(|n| n.parent.is_none())
        .filter_map(|n| n.inclusive_ms)
        .sum();
    let total_cost: f64 = out
        .iter()
        .filter(|n| n.parent.is_none())
        .filter_map(|n| n.node.total_cost)
        .sum();
    for n in &mut out {
        if total_ms > 0.0 {
            n.time_share = n.exclusive_ms.map(|e| (e / total_ms).clamp(0.0, 1.0));
        }
        if total_cost > 0.0 {
            n.cost_share = n.exclusive_cost.map(|e| (e / total_cost).clamp(0.0, 1.0));
        }
    }
    out
}

/// Node by path.
pub fn node_at<'a>(plan: &'a QueryPlan, path: &[usize]) -> Option<&'a PlanNode> {
    let (first, rest) = path.split_first()?;
    let mut node = plan.roots.get(*first)?;
    for &i in rest {
        node = node.children.get(i)?;
    }
    Some(node)
}

/// A node consuming a large share of the plan.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanHotspot {
    /// Node path.
    pub path: PlanNodePath,
    /// Node title.
    pub title: String,
    /// Exclusive time (ms) when analyzed.
    pub exclusive_ms: Option<f64>,
    /// Exclusive cost.
    pub exclusive_cost: Option<f64>,
    /// Share of the whole plan (time when analyzed, else cost), 0..=1.
    pub share: f64,
}

/// Category of a plan warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanWarningKind {
    /// Full scan of a table with many rows.
    SeqScanOnLargeTable,
    /// Actual rows differ from the estimate by 10x or more.
    RowEstimateOff,
    /// A sort spilled to disk.
    SortSpilledToDisk,
    /// A hash table did not fit into memory (several batches).
    HashBatches,
    /// A node was executed very many times (nested loop inner side).
    ManyLoops,
    /// A filter discarded most rows read.
    RowsRemovedByFilter,
}

/// A potential problem found in a plan.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanWarning {
    /// Node path.
    pub path: PlanNodePath,
    /// Category.
    pub kind: PlanWarningKind,
    /// Translated message.
    pub message: String,
}

/// Overview of a plan.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlanSummary {
    /// Planning time reported by the server.
    pub planning_ms: Option<f64>,
    /// Execution time reported by the server (or the root time).
    pub execution_ms: Option<f64>,
    /// Total estimated cost of the roots.
    pub total_cost: Option<f64>,
    /// Number of nodes.
    pub node_count: usize,
    /// Deepest level (roots are 0).
    pub max_depth: usize,
    /// Actual numbers are available.
    pub analyzed: bool,
    /// Most expensive nodes, most expensive first (at most 5, share >= 5 %).
    pub hotspots: Vec<PlanHotspot>,
    /// Potential problems.
    pub warnings: Vec<PlanWarning>,
}

/// Row count above which a full scan is reported.
pub const LARGE_TABLE_ROWS: f64 = 10_000.0;

fn property<'a>(node: &'a PlanNode, key: &str) -> Option<&'a str> {
    node.properties
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

fn property_number(node: &PlanNode, key: &str) -> Option<f64> {
    property(node, key)
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse().ok())
}

/// True for full table scans of the supported databases.
pub fn is_full_scan(node: &PlanNode) -> bool {
    let t = node.node_type.to_ascii_lowercase();
    t.contains("seq scan")
        || t.contains("table scan")
        || t.contains("full scan")
        || t == "all"
        || (t.starts_with("scan")
            && !t.contains("index")
            && !t.contains("constant")
            && !t.contains("subquery"))
}

fn fmt_rows(v: f64) -> String {
    crate::common::format::grouped(v.round())
}

/// Summarises a plan: times, hotspots and warnings.
pub fn plan_summary(plan: &QueryPlan) -> PlanSummary {
    let flat = flatten_plan(plan);
    let analyzed = plan.analyzed || has_actuals(plan);
    let metric = PlanMetric::Auto.resolve(plan);
    let root_ms: f64 = flat
        .iter()
        .filter(|n| n.parent.is_none())
        .filter_map(|n| n.inclusive_ms)
        .sum();
    let total_cost: f64 = flat
        .iter()
        .filter(|n| n.parent.is_none())
        .filter_map(|n| n.node.total_cost)
        .sum();
    let mut hot: Vec<&FlatPlanNode<'_>> = flat
        .iter()
        .filter(|n| n.share(metric).unwrap_or(0.0) >= 0.05)
        .collect();
    hot.sort_by(|a, b| {
        b.share(metric)
            .unwrap_or(0.0)
            .total_cmp(&a.share(metric).unwrap_or(0.0))
            .then(a.path.cmp(&b.path))
    });
    let hotspots = hot
        .into_iter()
        .take(5)
        .map(|n| PlanHotspot {
            path: n.path.clone(),
            title: n.node.title(),
            exclusive_ms: n.exclusive_ms,
            exclusive_cost: n.exclusive_cost,
            share: n.share(metric).unwrap_or(0.0),
        })
        .collect();

    let mut warnings = Vec::new();
    for n in &flat {
        let node = n.node;
        let rel = node
            .relation
            .clone()
            .unwrap_or_else(|| node.node_type.clone());
        let rows = node
            .plan_rows
            .unwrap_or(0.0)
            .max(n.output_rows().unwrap_or(0.0));
        let removed = property_number(node, "Rows Removed by Filter").unwrap_or(0.0)
            * node.loops.unwrap_or(1.0).max(1.0);
        if is_full_scan(node) && (rows + removed) >= LARGE_TABLE_ROWS {
            warnings.push(PlanWarning {
                path: n.path.clone(),
                kind: PlanWarningKind::SeqScanOnLargeTable,
                message: trf(
                    "Full scan of {} (~{} rows)",
                    &[&rel, &fmt_rows(rows + removed)],
                ),
            });
        }
        if let (Some(actual), Some(est), Some(f)) = (node.actual_rows, node.plan_rows, n.row_factor)
            && actual.max(est) >= 100.0
            && !(0.1..=10.0).contains(&f)
        {
            let factor = if f >= 1.0 { f } else { 1.0 / f };
            warnings.push(PlanWarning {
                path: n.path.clone(),
                kind: PlanWarningKind::RowEstimateOff,
                message: trf(
                    "Row estimate off by {}x at {}: estimated {}, actual {}",
                    &[
                        &format!("{factor:.0}"),
                        &node.node_type,
                        &fmt_rows(est),
                        &fmt_rows(actual),
                    ],
                ),
            });
        }
        let sort_method = property(node, "Sort Method")
            .unwrap_or("")
            .to_ascii_lowercase();
        let space_type = property(node, "Sort Space Type")
            .unwrap_or("")
            .to_ascii_lowercase();
        if sort_method.contains("external") || space_type == "disk" {
            warnings.push(PlanWarning {
                path: n.path.clone(),
                kind: PlanWarningKind::SortSpilledToDisk,
                message: trf(
                    "Sort spilled to disk ({}); consider more work_mem",
                    &[&property(node, "Sort Method").unwrap_or("external")],
                ),
            });
        }
        let batches = property_number(node, "Hash Batches").or_else(|| {
            if node.node_type.eq_ignore_ascii_case("hash") {
                property_number(node, "Batches")
            } else {
                None
            }
        });
        if batches.is_some_and(|b| b > 1.0) {
            warnings.push(PlanWarning {
                path: n.path.clone(),
                kind: PlanWarningKind::HashBatches,
                message: trf(
                    "Hash used {} batches: the hash table did not fit in memory",
                    &[&batches.unwrap_or(0.0)],
                ),
            });
        }
        if node.loops.is_some_and(|l| l >= 1000.0) && n.parent.is_some() {
            warnings.push(PlanWarning {
                path: n.path.clone(),
                kind: PlanWarningKind::ManyLoops,
                message: trf(
                    "{} executed {} times",
                    &[&node.title(), &fmt_rows(node.loops.unwrap_or(0.0))],
                ),
            });
        }
        let kept = n.output_rows().unwrap_or(0.0);
        if removed >= LARGE_TABLE_ROWS && removed >= 10.0 * kept.max(1.0) && !is_full_scan(node) {
            warnings.push(PlanWarning {
                path: n.path.clone(),
                kind: PlanWarningKind::RowsRemovedByFilter,
                message: trf(
                    "Filter discarded {} rows at {}; an index may help",
                    &[&fmt_rows(removed), &node.title()],
                ),
            });
        }
    }

    PlanSummary {
        planning_ms: plan.planning_ms,
        execution_ms: plan
            .execution_ms
            .or(if root_ms > 0.0 { Some(root_ms) } else { None }),
        total_cost: if total_cost > 0.0 {
            Some(total_cost)
        } else {
            None
        },
        node_count: flat.len(),
        max_depth: flat.iter().map(|n| n.depth).max().unwrap_or(0),
        analyzed,
        hotspots,
        warnings,
    }
}

/// One-line description of a summary for status bars.
pub fn summary_line(s: &PlanSummary) -> String {
    let mut parts = Vec::new();
    if let Some(p) = s.planning_ms {
        parts.push(trf(
            "Planning {}",
            &[&crate::common::format::duration_ms(p)],
        ));
    }
    if let Some(e) = s.execution_ms {
        parts.push(trf(
            "Execution {}",
            &[&crate::common::format::duration_ms(e)],
        ));
    }
    if let Some(c) = s.total_cost {
        parts.push(trf("Cost {}", &[&crate::common::format::compact(c, 2)]));
    }
    parts.push(trf("{} nodes", &[&s.node_count]));
    if !s.warnings.is_empty() {
        parts.push(trf("{} warnings", &[&s.warnings.len()]));
    }
    if parts.is_empty() {
        tr("Empty plan").to_string()
    } else {
        parts.join(" · ")
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sample_plan() -> QueryPlan {
        let mut scan = PlanNode::new("Seq Scan");
        scan.relation = Some("orders".into());
        scan.total_cost = Some(1800.0);
        scan.startup_cost = Some(0.0);
        scan.plan_rows = Some(100.0);
        scan.actual_rows = Some(50_000.0);
        scan.actual_total_ms = Some(60.0);
        scan.loops = Some(1.0);
        scan.properties = vec![("Rows Removed by Filter".into(), "150000".into())];
        let mut idx = PlanNode::new("Index Scan");
        idx.relation = Some("customers".into());
        idx.index = Some("customers_pkey".into());
        idx.total_cost = Some(8.0);
        idx.plan_rows = Some(1.0);
        idx.actual_rows = Some(1.0);
        idx.actual_total_ms = Some(0.002);
        idx.loops = Some(50_000.0);
        let mut nl = PlanNode::new("Nested Loop");
        nl.total_cost = Some(5000.0);
        nl.plan_rows = Some(100.0);
        nl.actual_rows = Some(50_000.0);
        nl.actual_total_ms = Some(200.0);
        nl.loops = Some(1.0);
        nl.children = vec![scan, idx];
        let mut sort = PlanNode::new("Sort");
        sort.total_cost = Some(6000.0);
        sort.actual_total_ms = Some(260.0);
        sort.actual_rows = Some(50_000.0);
        sort.plan_rows = Some(100.0);
        sort.loops = Some(1.0);
        sort.properties = vec![("Sort Method".into(), "external merge  Disk: 4096kB".into())];
        sort.children = vec![nl];
        QueryPlan {
            roots: vec![sort],
            planning_ms: Some(0.5),
            execution_ms: Some(262.0),
            analyzed: true,
            ..Default::default()
        }
    }

    #[test]
    fn flatten_and_shares() {
        let plan = sample_plan();
        let flat = flatten_plan(&plan);
        assert_eq!(flat.len(), 4);
        assert_eq!(flat[0].path, vec![0]);
        assert_eq!(flat[1].path, vec![0, 0]);
        assert_eq!(flat[2].path, vec![0, 0, 0]);
        assert_eq!(flat[3].path, vec![0, 0, 1]);
        assert_eq!(flat[1].children, vec![2, 3]);
        assert_eq!(flat[3].parent, Some(1));
        // index scan: 0.002 ms x 50,000 loops = 100 ms inclusive
        assert!((flat[3].inclusive_ms.unwrap() - 100.0).abs() < 1e-6);
        // nested loop exclusive: 200 - 60 - 100 = 40
        assert!((flat[1].exclusive_ms.unwrap() - 40.0).abs() < 1e-6);
        let sum: f64 = flat.iter().filter_map(|n| n.time_share).sum();
        assert!((sum - 1.0).abs() < 1e-6, "exclusive shares add up: {sum}");
        assert!(flat.iter().all(|n| n.cost_share.unwrap() >= 0.0));
        assert!(flat[2].row_factor.unwrap() > 100.0);
        assert_eq!(node_at(&plan, &[0, 0, 1]).unwrap().node_type, "Index Scan");
        assert!(node_at(&plan, &[0, 5]).is_none());
        assert!(node_at(&plan, &[]).is_none());
    }

    #[test]
    fn summary_finds_hotspots_and_warnings() {
        let plan = sample_plan();
        let s = plan_summary(&plan);
        assert_eq!(s.node_count, 4);
        assert_eq!(s.max_depth, 2);
        assert!(s.analyzed);
        assert_eq!(s.execution_ms, Some(262.0));
        assert_eq!(
            s.hotspots.first().map(|h| h.path.clone()),
            Some(vec![0, 0, 1]),
            "{:?}",
            s.hotspots
        );
        let kinds: Vec<PlanWarningKind> = s.warnings.iter().map(|w| w.kind).collect();
        assert!(kinds.contains(&PlanWarningKind::SeqScanOnLargeTable));
        assert!(kinds.contains(&PlanWarningKind::RowEstimateOff));
        assert!(kinds.contains(&PlanWarningKind::SortSpilledToDisk));
        assert!(kinds.contains(&PlanWarningKind::ManyLoops));
        assert!(!summary_line(&s).is_empty());
    }

    #[test]
    fn unanalyzed_plan_uses_cost() {
        let mut plan = sample_plan();
        fn strip(n: &mut PlanNode) {
            n.actual_rows = None;
            n.actual_total_ms = None;
            n.loops = None;
            n.children.iter_mut().for_each(strip);
        }
        plan.roots.iter_mut().for_each(strip);
        plan.analyzed = false;
        assert_eq!(PlanMetric::Auto.resolve(&plan), PlanMetric::Cost);
        let s = plan_summary(&plan);
        assert!(!s.analyzed);
        assert!(s.hotspots.iter().all(|h| h.exclusive_ms.is_none()));
        assert_eq!(plan_summary(&QueryPlan::default()).node_count, 0);
        assert!(is_full_scan(&PlanNode::new("ALL")));
        assert!(is_full_scan(&PlanNode::new("SCAN orders")));
        assert!(!is_full_scan(&PlanNode::new("SCAN orders USING INDEX idx")));
        assert!(!is_full_scan(&PlanNode::new("Index Only Scan")));
    }
}
