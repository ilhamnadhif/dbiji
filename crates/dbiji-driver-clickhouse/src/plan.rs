//! Conversion of ClickHouse `EXPLAIN` output into [`PlanNode`] trees.

use dbiji_core::plan::{PlanNode, QueryPlan};
use serde_json::Value as J;

/// Parse `EXPLAIN json = 1, ...` output (a JSON array with one `{"Plan": ...}`
/// object per query).
pub fn parse_json_plan(text: &str) -> Option<Vec<PlanNode>> {
    let v: J = serde_json::from_str(text.trim()).ok()?;
    let items = match v {
        J::Array(items) => items,
        other => vec![other],
    };
    let roots: Vec<PlanNode> = items
        .iter()
        .filter_map(|item| item.get("Plan").or(Some(item)).filter(|p| p.is_object()).map(node_from_json))
        .collect();
    if roots.is_empty() { None } else { Some(roots) }
}

fn scalar_text(v: &J) -> Option<String> {
    match v {
        J::String(s) => Some(s.clone()),
        J::Number(n) => Some(n.to_string()),
        J::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn compact(v: &J) -> String {
    match v {
        J::Array(items) => items
            .iter()
            .map(|i| match i {
                J::Object(o) => o
                    .get("Name")
                    .or_else(|| o.get("Column"))
                    .and_then(scalar_text)
                    .unwrap_or_else(|| i.to_string()),
                other => scalar_text(other).unwrap_or_else(|| other.to_string()),
            })
            .collect::<Vec<_>>()
            .join(", "),
        other => scalar_text(other).unwrap_or_else(|| other.to_string()),
    }
}

fn node_from_json(p: &J) -> PlanNode {
    let node_type = p.get("Node Type").and_then(scalar_text).unwrap_or_else(|| "Step".into());
    let mut node = PlanNode::new(node_type.clone());
    let Some(obj) = p.as_object() else { return node };
    for (k, v) in obj {
        match k.as_str() {
            "Node Type" | "Plans" | "Node Id" => {}
            "Description" => {
                let d = compact(v);
                if node_type.starts_with("ReadFrom") {
                    node.relation = Some(d.clone());
                }
                node.properties.push(("Description".into(), d));
            }
            "Expression" => {
                if let Some(outputs) = v.get("Outputs") {
                    node.properties.push(("Outputs".into(), compact(outputs)));
                }
            }
            "Filter Column" | "Filter column" => node.condition = Some(compact(v)),
            "Prewhere info" => {
                if let Some(f) = v.get("Prewhere filter").and_then(|pf| pf.get("Prewhere filter column")) {
                    node.condition = Some(compact(f));
                }
                node.properties.push(("Prewhere".into(), "yes".into()));
            }
            "Indexes" => {
                if let J::Array(idx) = v {
                    for (n, i) in idx.iter().enumerate() {
                        let ty = i.get("Type").and_then(scalar_text).unwrap_or_else(|| "Index".into());
                        let name = i.get("Name").and_then(scalar_text);
                        if n == 0 {
                            node.index = Some(name.clone().unwrap_or_else(|| ty.clone()));
                        }
                        let mut parts = Vec::new();
                        if let Some(c) = i.get("Condition").and_then(scalar_text) {
                            parts.push(format!("condition {c}"));
                        }
                        if let (Some(a), Some(b)) = (i.get("Selected Parts"), i.get("Initial Parts")) {
                            parts.push(format!("parts {}/{}", compact(a), compact(b)));
                        }
                        if let (Some(a), Some(b)) = (i.get("Selected Granules"), i.get("Initial Granules")) {
                            parts.push(format!("granules {}/{}", compact(a), compact(b)));
                        }
                        let label = match name {
                            Some(nm) => format!("Index {ty} {nm}"),
                            None => format!("Index {ty}"),
                        };
                        node.properties.push((label, parts.join(", ")));
                    }
                }
            }
            "Granules" => {
                node.properties.push(("Granules".into(), compact(v)));
            }
            _ => node.properties.push((k.clone(), compact(v))),
        }
    }
    if let Some(J::Array(children)) = obj.get("Plans") {
        node.children = children.iter().map(node_from_json).collect();
    }
    node
}

/// Parse plain `EXPLAIN PLAN` text (two spaces of indentation per level,
/// `Node (description)` lines) into a tree. Used when JSON output is not
/// available.
pub fn parse_text_plan(text: &str) -> Vec<PlanNode> {
    let mut roots: Vec<PlanNode> = Vec::new();
    // stack of (indent, node)
    let mut stack: Vec<(usize, PlanNode)> = Vec::new();
    for raw in text.lines() {
        let cleaned: String = raw.chars().map(|c| if "│└├─".contains(c) { ' ' } else { c }).collect();
        if cleaned.trim().is_empty() {
            continue;
        }
        let indent = cleaned.len() - cleaned.trim_start().len();
        let line = cleaned.trim();
        let is_property = line.contains(": ") || line.ends_with(':');
        if is_property
            && let Some((_, top)) = stack.last_mut() {
                let (k, v) = line.split_once(':').unwrap_or((line, ""));
                top.properties.push((k.trim().to_string(), v.trim().to_string()));
                continue;
            }
        let (name, desc) = match line.find(" (") {
            Some(i) if line.ends_with(')') => (&line[..i], Some(&line[i + 2..line.len() - 1])),
            _ => (line, None),
        };
        let mut node = PlanNode::new(name.trim());
        if let Some(d) = desc {
            if name.starts_with("ReadFrom") {
                node.relation = Some(d.to_string());
            }
            node.properties.push(("Description".into(), d.to_string()));
        }
        while let Some((ind, _)) = stack.last() {
            if *ind >= indent {
                let (_, done) = stack.pop().unwrap_or((0, PlanNode::default()));
                attach(&mut stack, &mut roots, done);
            } else {
                break;
            }
        }
        stack.push((indent, node));
    }
    while let Some((_, done)) = stack.pop() {
        attach(&mut stack, &mut roots, done);
    }
    roots
}

fn attach(stack: &mut [(usize, PlanNode)], roots: &mut Vec<PlanNode>, node: PlanNode) {
    match stack.last_mut() {
        Some((_, parent)) => parent.children.push(node),
        None => roots.push(node),
    }
}

/// Build the plan from the JSON text and the human readable text.
pub fn build_plan(json_text: Option<&str>, plain_text: Option<&str>) -> QueryPlan {
    let roots = json_text
        .and_then(parse_json_plan)
        .or_else(|| plain_text.map(parse_text_plan))
        .unwrap_or_default();
    let mut plan = QueryPlan { roots, raw: json_text.map(str::to_string), ..Default::default() };
    plan.text = match plain_text {
        Some(t) if !t.trim().is_empty() => t.to_string(),
        _ => plan.to_text_tree(),
    };
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON: &str = r#"[
  {
    "Plan": {
      "Node Type": "Expression",
      "Node Id": "Expression_9",
      "Description": "Project names",
      "Expression": {"Inputs": [], "Outputs": [{"Name": "name", "Type": "String"}, {"Name": "count()", "Type": "UInt64"}]},
      "Plans": [
        {
          "Node Type": "Aggregating",
          "Keys": ["__table1.name"],
          "Plans": [
            {
              "Node Type": "ReadFromMergeTree",
              "Description": "default.ex1",
              "Read Type": "Default",
              "Parts": 1,
              "Granules": 2,
              "Prewhere info": {"Need filter": true, "Prewhere filter": {"Prewhere filter column": "greater(__table1.id, 100_UInt8)"}},
              "Indexes": [
                {"Type": "PrimaryKey", "Keys": ["id"], "Condition": "(id in [101, +Inf))", "Initial Parts": 1, "Selected Parts": 1, "Initial Granules": 2, "Selected Granules": 1}
              ]
            }
          ]
        }
      ]
    }
  }
]"#;

    #[test]
    fn json_plan() {
        let roots = parse_json_plan(JSON).unwrap();
        assert_eq!(roots.len(), 1);
        let root = &roots[0];
        assert_eq!(root.node_type, "Expression");
        assert!(root.properties.iter().any(|(k, v)| k == "Outputs" && v == "name, count()"));
        let agg = &root.children[0];
        assert_eq!(agg.node_type, "Aggregating");
        assert!(agg.properties.iter().any(|(k, v)| k == "Keys" && v == "__table1.name"));
        let read = &agg.children[0];
        assert_eq!(read.relation.as_deref(), Some("default.ex1"));
        assert_eq!(read.condition.as_deref(), Some("greater(__table1.id, 100_UInt8)"));
        assert_eq!(read.index.as_deref(), Some("PrimaryKey"));
        assert!(read.properties.iter().any(|(k, v)| k == "Index PrimaryKey" && v.contains("granules 1/2")));
        assert_eq!(root.count(), 3);
    }

    #[test]
    fn text_plan() {
        let text = "Expression ((Project names + Projection))\n  Aggregating\n    Expression (Before GROUP BY)\n      ReadFromMergeTree (default.ex1)\n";
        let roots = parse_text_plan(text);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].node_type, "Expression");
        assert_eq!(roots[0].count(), 4);
        let read = &roots[0].children[0].children[0].children[0];
        assert_eq!(read.relation.as_deref(), Some("default.ex1"));
    }

    #[test]
    fn text_plan_with_properties_and_siblings() {
        let text = "Union\n  Expression\n    ReadFromStorage (SystemNumbers)\n  Expression\n    ReadFromStorage (SystemOne)\n";
        let roots = parse_text_plan(text);
        assert_eq!(roots[0].children.len(), 2);
        let text2 = "Aggregating\n│  Keys: name\n└──ReadFromMergeTree (default.ex1)\n      Read type: Default\n";
        let r2 = parse_text_plan(text2);
        assert_eq!(r2[0].node_type, "Aggregating");
        assert!(r2[0].properties.iter().any(|(k, _)| k == "Keys"));
        assert_eq!(r2[0].children[0].node_type, "ReadFromMergeTree");
    }

    #[test]
    fn plan_builder_falls_back() {
        let p = build_plan(Some("not json"), Some("Expression\n  ReadFromStorage (SystemOne)\n"));
        assert_eq!(p.roots.len(), 1);
        assert!(p.text.contains("ReadFromStorage"));
        let p = build_plan(Some(JSON), None);
        assert!(p.text.contains("ReadFromMergeTree on default.ex1"), "{}", p.text);
    }
}
