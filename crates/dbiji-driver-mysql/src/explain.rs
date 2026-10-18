//! Execution plans: `EXPLAIN FORMAT=JSON` (MySQL 5.7+/8.x, both JSON format
//! versions, and MariaDB including `ANALYZE FORMAT=JSON`), `EXPLAIN
//! FORMAT=TREE` / `EXPLAIN ANALYZE` text trees (MySQL 8.0.16+ / 8.0.18+) and
//! the classic tabular `EXPLAIN` as fallback.

use crate::convert::read;
use crate::session::MySqlSession;
use crate::sqltext;
use dbiji_core::dialect::ansi;
use dbiji_core::plan::{PlanNode, QueryPlan};
use dbiji_core::{DbError, DbResult, Session, TransactionMode};
use serde_json::{Map, Value};

/// Keys whose object value is a plan operation.
const CONTAINERS: &[&str] = &[
    "query_block",
    "table",
    "ordering_operation",
    "grouping_operation",
    "duplicates_removal",
    "union_result",
    "windowing",
    "buffer_result",
    "block-nl-join",
    "filesort",
    "temporary_table",
    "read_sorted_file",
    "materialized_from_subquery",
    "materialized",
    "expression_cache",
    "subquery",
    "insert_from",
    "range-checked-for-each-record",
    "duplicate_removal",
];

/// Keys whose array value lists sub-queries.
const SUBQUERY_ARRAYS: &[&str] = &[
    "attached_subqueries",
    "optimized_away_subqueries",
    "select_list_subqueries",
    "having_subqueries",
    "order_by_subqueries",
    "group_by_subqueries",
    "update_value_subqueries",
    "subqueries",
    "query_specifications",
];

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn scalar_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Array(items) if items.iter().all(|i| !i.is_object() && !i.is_array()) => Some(
            items
                .iter()
                .filter_map(scalar_text)
                .collect::<Vec<_>>()
                .join(", "),
        ),
        _ => None,
    }
}

fn get_f64(o: &Map<String, Value>, k: &str) -> Option<f64> {
    o.get(k).and_then(num)
}

fn label(key: &str, o: &Map<String, Value>) -> String {
    match key {
        "query_block" => match o.get("select_id").and_then(scalar_text) {
            Some(id) => format!("Query block #{id}"),
            None => "Query block".into(),
        },
        "table" => {
            if let Some(a) = o.get("access_type").and_then(|v| v.as_str()) {
                a.to_string()
            } else if let Some(m) = o.get("message").and_then(|v| v.as_str()) {
                m.to_string()
            } else {
                "Table".into()
            }
        }
        "ordering_operation" => {
            if o.get("using_filesort")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                "Sort (filesort)".into()
            } else {
                "Ordering".into()
            }
        }
        "grouping_operation" => {
            if o.get("using_temporary_table")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                "Group (temporary table)".into()
            } else {
                "Group".into()
            }
        }
        "duplicates_removal" | "duplicate_removal" => "Duplicates removal".into(),
        "union_result" => "Union result".into(),
        "windowing" => "Window".into(),
        "buffer_result" => "Buffer result".into(),
        "block-nl-join" => "Block nested loop join".into(),
        "filesort" => "Filesort".into(),
        "temporary_table" => "Temporary table".into(),
        "read_sorted_file" => "Read sorted file".into(),
        "materialized_from_subquery" | "materialized" => "Materialize".into(),
        "expression_cache" => "Expression cache".into(),
        "subquery" => "Subquery".into(),
        "insert_from" => "Insert from".into(),
        "range-checked-for-each-record" => "Range checked for each record".into(),
        other => other.replace('_', " "),
    }
}

/// Flatten scalar members (and one level of nested non-plan objects) into properties.
fn fill_properties(node: &mut PlanNode, prefix: &str, o: &Map<String, Value>) {
    for (k, v) in o {
        if CONTAINERS.contains(&k.as_str())
            || SUBQUERY_ARRAYS.contains(&k.as_str())
            || k == "nested_loop"
        {
            continue;
        }
        let name = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        match v {
            Value::Object(inner) if prefix.is_empty() => fill_properties(node, k, inner),
            Value::Object(_) | Value::Null => {}
            other => {
                if let Some(t) = scalar_text(other) {
                    node.properties.push((name, t));
                }
            }
        }
    }
}

fn plan_node(key: &str, v: &Value) -> PlanNode {
    let Some(o) = v.as_object() else {
        let mut n = PlanNode::new(label(key, &Map::new()));
        if let Some(t) = scalar_text(v) {
            n.properties.push((key.to_string(), t));
        }
        return n;
    };
    let mut node = PlanNode::new(label(key, o));
    node.relation = o
        .get("table_name")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    node.index = o.get("key").and_then(|v| v.as_str()).map(str::to_string);
    node.condition = [
        "attached_condition",
        "having_condition",
        "sort_key",
        "condition",
    ]
    .iter()
    .find_map(|k| o.get(*k).and_then(|v| v.as_str()))
    .map(str::to_string);
    // estimates
    node.plan_rows = get_f64(o, "rows_produced_per_join").or_else(|| get_f64(o, "rows"));
    if let Some(ci) = o.get("cost_info").and_then(|v| v.as_object()) {
        node.total_cost = get_f64(ci, "query_cost")
            .or_else(|| get_f64(ci, "sort_cost"))
            .or_else(
                || match (get_f64(ci, "read_cost"), get_f64(ci, "eval_cost")) {
                    (Some(r), Some(e)) => Some(r + e),
                    (Some(r), None) => Some(r),
                    (None, Some(e)) => Some(e),
                    _ => get_f64(ci, "prefix_cost"),
                },
            );
    } else if let Some(c) = get_f64(o, "cost") {
        node.total_cost = Some(c);
    }
    // MariaDB ANALYZE values (r_*): totals are converted to per-loop averages.
    let loops = get_f64(o, "r_loops");
    if loops.is_some() {
        node.loops = loops;
    }
    let total_ms = get_f64(o, "r_total_time_ms").or_else(|| {
        match (get_f64(o, "r_table_time_ms"), get_f64(o, "r_other_time_ms")) {
            (Some(a), Some(b)) => Some(a + b),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            _ => None,
        }
    });
    if let Some(t) = total_ms {
        let l = loops.filter(|l| *l > 0.0).unwrap_or(1.0);
        node.actual_total_ms = Some(t / l);
    }
    node.actual_rows = get_f64(o, "r_rows").or_else(|| get_f64(o, "r_output_rows"));
    fill_properties(&mut node, "", o);

    for (k, val) in o {
        match (k.as_str(), val) {
            ("nested_loop", Value::Array(items)) => {
                let mut nl = PlanNode::new("Nested loop");
                for item in items {
                    if let Some(io) = item.as_object() {
                        for (ik, iv) in io {
                            nl.children.push(plan_node(ik, iv));
                        }
                    }
                }
                // MySQL: the cost of the join is the prefix cost of its last table.
                nl.total_cost = nl.children.last().and_then(|c| {
                    c.properties
                        .iter()
                        .find(|(k, _)| k == "cost_info.prefix_cost")
                        .and_then(|(_, v)| v.parse().ok())
                });
                nl.plan_rows = nl.children.last().and_then(|c| c.plan_rows);
                node.children.push(nl);
            }
            (k, Value::Object(_)) if CONTAINERS.contains(&k) => {
                node.children.push(plan_node(k, val))
            }
            (k, Value::Array(items)) if SUBQUERY_ARRAYS.contains(&k) => {
                for item in items {
                    let Some(io) = item.as_object() else { continue };
                    let mut sub = PlanNode::new(if k == "query_specifications" {
                        "Union member".to_string()
                    } else {
                        format!("Subquery ({})", k.replace('_', " "))
                    });
                    fill_properties(&mut sub, "", io);
                    for (ik, iv) in io {
                        if CONTAINERS.contains(&ik.as_str()) {
                            sub.children.push(plan_node(ik, iv));
                        }
                    }
                    // a member that only wraps one query block: flatten
                    if sub.children.len() == 1
                        && sub
                            .properties
                            .iter()
                            .all(|(k, _)| k == "dependent" || k == "cacheable")
                    {
                        let mut only = sub.children.remove(0);
                        only.properties.extend(std::mem::take(&mut sub.properties));
                        node.children.push(only);
                    } else {
                        node.children.push(sub);
                    }
                }
            }
            _ => {}
        }
    }
    node
}

/// MySQL `explain_json_format_version=2` node (`operation` / `inputs`).
fn v2_node(v: &Value) -> PlanNode {
    let o = match v.as_object() {
        Some(o) => o,
        None => return PlanNode::new("?"),
    };
    let mut node = PlanNode::new(
        o.get("operation")
            .and_then(|v| v.as_str())
            .unwrap_or("Operation"),
    );
    node.relation = o
        .get("table_name")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    node.alias = o.get("alias").and_then(|v| v.as_str()).map(str::to_string);
    node.index = o
        .get("index_name")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    node.condition = o
        .get("condition")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    node.total_cost = get_f64(o, "estimated_total_cost");
    node.startup_cost = get_f64(o, "estimated_first_row_cost");
    node.plan_rows = get_f64(o, "estimated_rows");
    node.actual_rows = get_f64(o, "actual_rows");
    node.loops = get_f64(o, "actual_loops");
    node.actual_total_ms = get_f64(o, "actual_last_row_ms");
    node.actual_startup_ms = get_f64(o, "actual_first_row_ms");
    for (k, val) in o {
        if k == "inputs" || k == "query" {
            continue;
        }
        if let Some(t) = scalar_text(val) {
            node.properties.push((k.clone(), t));
        }
    }
    if let Some(Value::Array(inputs)) = o.get("inputs") {
        node.children = inputs.iter().map(v2_node).collect();
    }
    node
}

/// Parse `EXPLAIN FORMAT=JSON` / `ANALYZE FORMAT=JSON` output.
pub fn parse_json_plan(json: &str) -> DbResult<(Vec<PlanNode>, Option<f64>, Option<f64>)> {
    let v: Value = serde_json::from_str(json)
        .map_err(|e| DbError::query(format!("Cannot parse the JSON plan: {e}")))?;
    let Some(o) = v.as_object() else {
        return Err(DbError::query("Unexpected JSON plan format"));
    };
    if o.contains_key("operation") || o.contains_key("inputs") {
        return Ok((vec![v2_node(&v)], None, None));
    }
    let mut roots = Vec::new();
    let mut planning = None;
    let mut execution = None;
    for (k, val) in o {
        match k.as_str() {
            "query_optimization" => {
                planning = val.as_object().and_then(|q| get_f64(q, "r_total_time_ms"))
            }
            "query_block" => {
                execution = val.as_object().and_then(|q| get_f64(q, "r_total_time_ms"));
                roots.push(plan_node(k, val));
            }
            k if CONTAINERS.contains(&k) => roots.push(plan_node(k, val)),
            _ => {}
        }
    }
    Ok((roots, planning, execution))
}

fn parse_range(v: &str) -> (Option<f64>, Option<f64>) {
    match v.split_once("..") {
        Some((a, b)) => (a.trim().parse().ok(), b.trim().parse().ok()),
        None => (None, v.trim().parse().ok()),
    }
}

fn parse_tree_line(text: &str) -> PlanNode {
    let mut s = text.trim().to_string();
    let mut node = PlanNode::default();
    if let Some(stripped) = s.strip_suffix("(never executed)") {
        node.properties
            .push(("never_executed".into(), "true".into()));
        node.actual_rows = Some(0.0);
        node.loops = Some(0.0);
        s = stripped.trim_end().to_string();
    }
    if let Some(i) = s.rfind("(actual time=")
        && s.ends_with(')')
    {
        let inner = s[i + 1..s.len() - 1]
            .trim_start_matches("actual ")
            .to_string();
        for part in inner.split_whitespace() {
            match part.split_once('=') {
                Some(("time", v)) => {
                    let (a, b) = parse_range(v);
                    node.actual_startup_ms = a;
                    node.actual_total_ms = b;
                }
                Some(("rows", v)) => node.actual_rows = v.parse().ok(),
                Some(("loops", v)) => node.loops = v.parse().ok(),
                _ => {}
            }
        }
        s.truncate(i);
        s = s.trim_end().to_string();
    }
    if let Some(i) = s.rfind("(cost=") {
        if s.ends_with(')') {
            let inner = s[i + 1..s.len() - 1].to_string();
            for part in inner.split_whitespace() {
                match part.split_once('=') {
                    Some(("cost", v)) => {
                        let (a, b) = parse_range(v);
                        node.startup_cost = a;
                        node.total_cost = b;
                    }
                    Some(("rows", v)) => node.plan_rows = v.parse().ok(),
                    _ => {}
                }
            }
            s.truncate(i);
            s = s.trim_end().to_string();
        }
    } else if let Some(i) = s.rfind("(rows=")
        && s.ends_with(')')
    {
        node.plan_rows = s[i + 6..s.len() - 1].trim().parse().ok();
        s.truncate(i);
        s = s.trim_end().to_string();
    }
    let desc = s.trim().to_string();
    node.properties
        .insert(0, ("description".into(), desc.clone()));
    split_description(&mut node, &desc);
    node
}

fn split_description(node: &mut PlanNode, desc: &str) {
    if let Some((head, tail)) = desc.split_once(": ")
        && !head.contains(" on ")
        && !head.contains('(')
    {
        node.node_type = head.to_string();
        node.condition = Some(tail.to_string());
        return;
    }
    if let Some(pos) = desc.find(" on ") {
        let head = &desc[..pos];
        let hl = head.to_ascii_lowercase();
        if hl.ends_with("scan")
            || hl.ends_with("lookup")
            || hl.ends_with("search")
            || hl.ends_with("seek")
        {
            node.node_type = head.to_string();
            let rest = desc[pos + 4..].trim();
            let (rel, mut after) = match rest.find(' ') {
                Some(i) => (&rest[..i], rest[i..].trim()),
                None => (rest, ""),
            };
            node.relation = Some(rel.trim_matches('`').to_string());
            if let Some(a) = after.strip_prefix("using ") {
                let (idx, rest2) = match a.find(' ') {
                    Some(i) => (&a[..i], a[i..].trim()),
                    None => (a, ""),
                };
                node.index = Some(idx.trim_matches('`').to_string());
                after = rest2;
            }
            let cond = after.strip_prefix("over ").unwrap_or(after).trim();
            if !cond.is_empty() {
                node.condition = Some(cond.to_string());
            }
            return;
        }
    }
    if desc.ends_with(')')
        && let Some(i) = desc.find(" (")
    {
        node.node_type = desc[..i].to_string();
        node.condition = Some(desc[i + 1..].to_string());
        return;
    }
    node.node_type = desc.to_string();
}

/// Parse `EXPLAIN FORMAT=TREE` / `EXPLAIN ANALYZE` output into nodes.
pub fn parse_tree_plan(text: &str) -> Vec<PlanNode> {
    fn attach(roots: &mut Vec<PlanNode>, stack: &mut [(usize, PlanNode)], node: PlanNode) {
        match stack.last_mut() {
            Some((_, parent)) => parent.children.push(node),
            None => roots.push(node),
        }
    }
    let mut roots: Vec<PlanNode> = Vec::new();
    let mut stack: Vec<(usize, PlanNode)> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let t = line.trim_start();
        let Some(rest) = t.strip_prefix("->") else {
            // continuation of the previous node's description
            if let Some((_, n)) = stack.last_mut() {
                match &mut n.condition {
                    Some(c) => {
                        c.push(' ');
                        c.push_str(t.trim());
                    }
                    None => n.node_type.push_str(&format!(" {}", t.trim())),
                }
            }
            continue;
        };
        let node = parse_tree_line(rest);
        while stack.last().is_some_and(|(ind, _)| *ind >= indent) {
            if let Some((_, done)) = stack.pop() {
                attach(&mut roots, &mut stack, done);
            }
        }
        stack.push((indent, node));
    }
    while let Some((_, done)) = stack.pop() {
        attach(&mut roots, &mut stack, done);
    }
    roots
}

/// Classic tabular EXPLAIN rows → flat nodes (one per row).
pub fn tabular_nodes(columns: &[String], rows: &[Vec<Option<String>>]) -> Vec<PlanNode> {
    let col = |name: &str| columns.iter().position(|c| c.eq_ignore_ascii_case(name));
    let (c_type, c_table, c_key, c_rows, c_extra, c_rrows) = (
        col("type"),
        col("table"),
        col("key"),
        col("rows"),
        col("Extra"),
        col("r_rows"),
    );
    rows.iter()
        .map(|r| {
            let get = |i: Option<usize>| i.and_then(|i| r.get(i).cloned().flatten());
            let mut n = PlanNode::new(get(c_type).unwrap_or_else(|| "?".into()));
            n.relation = get(c_table);
            n.index = get(c_key);
            n.plan_rows = get(c_rows).and_then(|v| v.parse().ok());
            n.actual_rows = get(c_rrows).and_then(|v| v.parse().ok());
            n.condition = get(c_extra);
            for (i, c) in columns.iter().enumerate() {
                if let Some(Some(v)) = r.get(i) {
                    n.properties.push((c.clone(), v.clone()));
                }
            }
            n
        })
        .collect()
}

/// Render rows as an aligned text table.
pub fn tabular_text(columns: &[String], rows: &[Vec<Option<String>>]) -> String {
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            r.iter()
                .map(|c| c.clone().unwrap_or_else(|| "NULL".into()))
                .collect()
        })
        .collect();
    let mut widths: Vec<usize> = columns.iter().map(|c| c.chars().count()).collect();
    for r in &cells {
        for (i, c) in r.iter().enumerate() {
            if let Some(w) = widths.get_mut(i) {
                *w = (*w).max(c.chars().count());
            }
        }
    }
    let line = |vals: &[String]| {
        vals.iter()
            .enumerate()
            .map(|(i, v)| format!("{v:<w$}", w = widths.get(i).copied().unwrap_or(0)))
            .collect::<Vec<_>>()
            .join(" | ")
    };
    let mut out = line(columns);
    out.push('\n');
    out.push_str(
        &widths
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("-+-"),
    );
    out.push('\n');
    for r in &cells {
        out.push_str(&line(r));
        out.push('\n');
    }
    out
}

impl MySqlSession {
    async fn explain_text(&mut self, sql: &str) -> DbResult<String> {
        let rows = self.rows(sql, Vec::new()).await?;
        Ok(rows
            .iter()
            .filter_map(|r| read::text(r, 0))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    async fn explain_tabular(&mut self, stmt: &str, prefix: &str) -> DbResult<QueryPlan> {
        let (cols, rows) = self.rows_with_columns(&format!("{prefix} {stmt}")).await?;
        let rows: Vec<Vec<Option<String>>> = rows
            .iter()
            .map(|r| (0..cols.len()).map(|i| read::text(r, i)).collect())
            .collect();
        Ok(QueryPlan {
            roots: tabular_nodes(&cols, &rows),
            text: tabular_text(&cols, &rows),
            raw: None,
            planning_ms: None,
            execution_ms: None,
            analyzed: prefix.starts_with("ANALYZE"),
        })
    }

    async fn explain_inner(&mut self, stmt: &str, analyze: bool) -> DbResult<QueryPlan> {
        if analyze {
            if self.profile.is_mariadb() {
                let json = self
                    .explain_text(&format!("ANALYZE FORMAT=JSON {stmt}"))
                    .await;
                return match json {
                    Ok(json) => {
                        let (roots, planning, execution) = parse_json_plan(&json)?;
                        let mut plan = QueryPlan {
                            roots,
                            text: String::new(),
                            raw: Some(json),
                            planning_ms: planning,
                            execution_ms: execution,
                            analyzed: true,
                        };
                        plan.text = plan.to_text_tree();
                        Ok(plan)
                    }
                    Err(e) if e.kind == dbiji_core::ErrorKind::Syntax => {
                        self.explain_tabular(stmt, "ANALYZE").await
                    }
                    Err(e) => Err(e),
                };
            }
            let text = self
                .explain_text(&format!("EXPLAIN ANALYZE {stmt}"))
                .await?;
            let roots = parse_tree_plan(&text);
            let execution = roots.first().and_then(|r| r.actual_total_ms);
            return Ok(QueryPlan {
                roots,
                text,
                raw: None,
                planning_ms: None,
                execution_ms: execution,
                analyzed: true,
            });
        }
        if self.profile.has_explain_json() {
            match self
                .explain_text(&format!("EXPLAIN FORMAT=JSON {stmt}"))
                .await
            {
                Ok(json) => {
                    let (roots, _, _) = parse_json_plan(&json)?;
                    let mut plan = QueryPlan {
                        roots,
                        text: String::new(),
                        raw: Some(json),
                        planning_ms: None,
                        execution_ms: None,
                        analyzed: false,
                    };
                    if self.profile.has_explain_tree()
                        && let Ok(tree) = self
                            .explain_text(&format!("EXPLAIN FORMAT=TREE {stmt}"))
                            .await
                        && !tree.trim().is_empty()
                        && !tree.contains("not executable by iterator executor")
                    {
                        plan.text = tree;
                    }
                    if plan.text.is_empty() {
                        plan.text = plan.to_text_tree();
                    }
                    return Ok(plan);
                }
                Err(e) if e.kind == dbiji_core::ErrorKind::Connection => return Err(e),
                Err(e) => {
                    tracing::debug!("EXPLAIN FORMAT=JSON failed, using the tabular format: {e}")
                }
            }
        }
        self.explain_tabular(stmt, "EXPLAIN").await
    }

    pub(crate) async fn run_explain(&mut self, sql: &str, analyze: bool) -> DbResult<QueryPlan> {
        let stmt = ansi::strip_trailing_semicolon(sql).to_string();
        if stmt.is_empty() {
            return Err(DbError::query("Nothing to explain"));
        }
        if analyze && !self.profile.has_explain_analyze() {
            return Err(DbError::unsupported(
                "EXPLAIN ANALYZE requires MySQL 8.0.18 or later (or MariaDB 10.1 or later)",
            ));
        }
        // ANALYZE executes the statement: undo data changes afterwards.
        let writes = analyze && !sqltext::is_read_only_statement(&stmt);
        if !writes {
            return self.explain_inner(&stmt, analyze).await;
        }
        let use_savepoint =
            self.in_transaction() || self.transaction_mode() == TransactionMode::Manual;
        if use_savepoint {
            self.exec_simple("SAVEPOINT dbiji_explain").await?;
        } else {
            self.exec_simple("START TRANSACTION").await?;
        }
        let res = self.explain_inner(&stmt, analyze).await;
        let undo = if use_savepoint {
            "ROLLBACK TO SAVEPOINT dbiji_explain"
        } else {
            "ROLLBACK"
        };
        let undo_res = self.exec_simple(undo).await;
        if use_savepoint {
            let _ = self.exec_simple("RELEASE SAVEPOINT dbiji_explain").await;
        }
        let plan = res?;
        undo_res?;
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MYSQL_JSON: &str = r#"{
  "query_block": {
    "select_id": 1,
    "cost_info": {"query_cost": "0.90"},
    "ordering_operation": {
      "using_filesort": true,
      "grouping_operation": {
        "using_temporary_table": true,
        "using_filesort": false,
        "nested_loop": [
          {"table": {"table_name": "a", "access_type": "ALL", "possible_keys": ["PRIMARY"], "rows_examined_per_scan": 3,
                     "rows_produced_per_join": 1, "filtered": "33.33",
                     "cost_info": {"read_cost": "0.45", "eval_cost": "0.10", "prefix_cost": "0.55", "data_read_per_join": "96"},
                     "used_columns": ["id", "v", "x"], "attached_condition": "(`db`.`a`.`x` > 0)"}},
          {"table": {"table_name": "b", "access_type": "ref", "possible_keys": ["a_id"], "key": "a_id", "used_key_parts": ["a_id"],
                     "key_length": "5", "ref": ["db.a.id"], "rows_examined_per_scan": 1, "rows_produced_per_join": 1,
                     "filtered": "100.00", "using_index": true,
                     "cost_info": {"read_cost": "0.25", "eval_cost": "0.10", "prefix_cost": "0.90", "data_read_per_join": "16"},
                     "used_columns": ["id", "a_id"]}}
        ]
      }
    }
  }
}"#;

    #[test]
    fn mysql_json() {
        let (roots, _, _) = parse_json_plan(MYSQL_JSON).unwrap();
        assert_eq!(roots.len(), 1);
        let qb = &roots[0];
        assert_eq!(qb.node_type, "Query block #1");
        assert_eq!(qb.total_cost, Some(0.90));
        let sort = &qb.children[0];
        assert_eq!(sort.node_type, "Sort (filesort)");
        let group = &sort.children[0];
        assert_eq!(group.node_type, "Group (temporary table)");
        let nl = &group.children[0];
        assert_eq!(nl.node_type, "Nested loop");
        assert_eq!(nl.total_cost, Some(0.90));
        assert_eq!(nl.children.len(), 2);
        let a = &nl.children[0];
        assert_eq!(
            (a.node_type.as_str(), a.relation.as_deref()),
            ("ALL", Some("a"))
        );
        assert_eq!(a.plan_rows, Some(1.0));
        assert!((a.total_cost.unwrap() - 0.55).abs() < 1e-9);
        assert_eq!(a.condition.as_deref(), Some("(`db`.`a`.`x` > 0)"));
        assert!(
            a.properties
                .iter()
                .any(|(k, v)| k == "possible_keys" && v == "PRIMARY")
        );
        assert!(
            a.properties
                .iter()
                .any(|(k, v)| k == "cost_info.prefix_cost" && v == "0.55")
        );
        let b = &nl.children[1];
        assert_eq!(
            (b.node_type.as_str(), b.index.as_deref()),
            ("ref", Some("a_id"))
        );
        assert!(
            b.properties
                .iter()
                .any(|(k, v)| k == "used_key_parts" && v == "a_id")
        );
    }

    #[test]
    fn mysql_union_and_subqueries() {
        let json = r#"{"query_block": {"union_result": {"using_temporary_table": true, "select_id": 5, "table_name": "<union1,3>",
            "access_type": "ALL", "query_specifications": [
              {"dependent": false, "cacheable": true, "query_block": {"select_id": 1, "cost_info": {"query_cost": "1.60"},
                  "table": {"table_name": "a", "access_type": "ALL"}}},
              {"dependent": false, "cacheable": true, "query_block": {"select_id": 3, "cost_info": {"query_cost": "0.55"},
                  "table": {"table_name": "a", "access_type": "ALL", "attached_condition": "x",
                     "attached_subqueries": [{"dependent": false, "cacheable": true, "query_block": {"select_id": 4,
                        "table": {"table_name": "a", "access_type": "ALL"}}}]}}}]}}}"#;
        let (roots, _, _) = parse_json_plan(json).unwrap();
        let union = &roots[0].children[0];
        assert_eq!(union.node_type, "Union result");
        assert_eq!(union.relation.as_deref(), Some("<union1,3>"));
        assert_eq!(union.children.len(), 2);
        assert_eq!(union.children[0].node_type, "Query block #1");
        let t = &union.children[1].children[0];
        assert_eq!(t.node_type, "ALL");
        assert_eq!(t.children[0].node_type, "Query block #4");
        assert_eq!(roots[0].count(), 8);
        let msg = r#"{"query_block": {"select_id": 1, "message": "No tables used"}}"#;
        let (r, _, _) = parse_json_plan(msg).unwrap();
        assert!(
            r[0].properties
                .iter()
                .any(|(k, v)| k == "message" && v == "No tables used")
        );
    }

    #[test]
    fn mariadb_analyze_json() {
        let json = r#"{"query_optimization": {"r_total_time_ms": 0.11},
          "query_block": {"select_id": 1, "cost": 0.0106, "r_loops": 1, "r_total_time_ms": 0.12,
            "filesort": {"sort_key": "count(0) desc", "r_loops": 1, "r_total_time_ms": 0.0067, "r_output_rows": 2,
              "temporary_table": {"nested_loop": [
                {"table": {"table_name": "b", "access_type": "index", "key": "a_id", "loops": 1, "r_loops": 1, "rows": 3, "r_rows": 3,
                  "cost": 0.007, "r_table_time_ms": 0.017, "r_other_time_ms": 0.016, "r_engine_stats": {"pages_accessed": 1},
                  "filtered": 100, "r_filtered": 100, "attached_condition": "b.a_id is not null"}},
                {"table": {"table_name": "a", "access_type": "eq_ref", "key": "PRIMARY", "loops": 3, "r_loops": 3, "rows": 1, "r_rows": 1,
                  "r_table_time_ms": 0.03, "r_other_time_ms": 0.03}}]}}}}"#;
        let (roots, planning, execution) = parse_json_plan(json).unwrap();
        assert_eq!(planning, Some(0.11));
        assert_eq!(execution, Some(0.12));
        let fs = &roots[0].children[0];
        assert_eq!(fs.node_type, "Filesort");
        assert_eq!(fs.condition.as_deref(), Some("count(0) desc"));
        assert_eq!(fs.actual_rows, Some(2.0));
        let tt = &fs.children[0];
        assert_eq!(tt.node_type, "Temporary table");
        let nl = &tt.children[0];
        let b = &nl.children[0];
        assert_eq!(
            (b.node_type.as_str(), b.plan_rows, b.actual_rows, b.loops),
            ("index", Some(3.0), Some(3.0), Some(1.0))
        );
        assert!((b.actual_total_ms.unwrap() - 0.033).abs() < 1e-9);
        assert!(
            b.properties
                .iter()
                .any(|(k, v)| k == "r_engine_stats.pages_accessed" && v == "1")
        );
        let a = &nl.children[1];
        assert_eq!(a.loops, Some(3.0));
        assert!(
            (a.actual_total_ms.unwrap() - 0.02).abs() < 1e-9,
            "per-loop average"
        );
    }

    #[test]
    fn json_v2() {
        let json = r#"{"query": "/* select#1 */ select ...", "operation": "Nested loop inner join", "estimated_rows": 1.5,
            "estimated_total_cost": 0.95, "inputs": [
              {"operation": "Table scan on a", "table_name": "a", "access_type": "table", "estimated_rows": 3, "estimated_total_cost": 0.55},
              {"operation": "Index lookup on b using a_id (a_id = a.id)", "table_name": "b", "index_name": "a_id",
               "access_type": "index", "estimated_rows": 1.5, "actual_rows": 1, "actual_loops": 3, "actual_last_row_ms": 0.005}]}"#;
        let (roots, _, _) = parse_json_plan(json).unwrap();
        assert_eq!(roots[0].node_type, "Nested loop inner join");
        assert_eq!(roots[0].children.len(), 2);
        assert_eq!(roots[0].children[1].index.as_deref(), Some("a_id"));
        assert_eq!(roots[0].children[1].loops, Some(3.0));
    }

    const TREE: &str = "-> Sort: `COUNT(*)` DESC  (actual time=0.0831..0.0833 rows=2 loops=1)\n    -> Table scan on <temporary>  (actual time=0.0736..0.074 rows=2 loops=1)\n        -> Aggregate using temporary table  (actual time=0.0729..0.0729 rows=2 loops=1)\n            -> Nested loop inner join  (cost=0.95 rows=1.5) (actual time=0.0319..0.0404 rows=3 loops=1)\n                -> Filter: (a.x > 0)  (cost=0.55 rows=1) (actual time=0.0209..0.0224 rows=3 loops=1)\n                    -> Table scan on a  (cost=0.55 rows=3) (actual time=0.0194..0.0205 rows=3 loops=1)\n                -> Covering index lookup on b using a_id (a_id=a.id)  (cost=0.4 rows=1.5) (actual time=0.00349..0.00483 rows=1 loops=3)\n";

    #[test]
    fn tree_analyze() {
        let roots = parse_tree_plan(TREE);
        assert_eq!(roots.len(), 1);
        let sort = &roots[0];
        assert_eq!(sort.node_type, "Sort");
        assert_eq!(sort.condition.as_deref(), Some("`COUNT(*)` DESC"));
        assert_eq!(sort.actual_total_ms, Some(0.0833));
        assert_eq!(sort.count(), 7);
        let scan = &sort.children[0];
        assert_eq!(
            (scan.node_type.as_str(), scan.relation.as_deref()),
            ("Table scan", Some("<temporary>"))
        );
        let nl = &scan.children[0].children[0];
        assert_eq!(nl.node_type, "Nested loop inner join");
        assert_eq!(
            (nl.total_cost, nl.plan_rows, nl.actual_rows),
            (Some(0.95), Some(1.5), Some(3.0))
        );
        assert_eq!(nl.children.len(), 2);
        let filter = &nl.children[0];
        assert_eq!(
            (filter.node_type.as_str(), filter.condition.as_deref()),
            ("Filter", Some("(a.x > 0)"))
        );
        assert_eq!(filter.children[0].relation.as_deref(), Some("a"));
        let lookup = &nl.children[1];
        assert_eq!(lookup.node_type, "Covering index lookup");
        assert_eq!(
            (lookup.relation.as_deref(), lookup.index.as_deref()),
            (Some("b"), Some("a_id"))
        );
        assert_eq!(lookup.condition.as_deref(), Some("(a_id=a.id)"));
        assert_eq!(lookup.loops, Some(3.0));
    }

    #[test]
    fn tree_estimates_and_misc() {
        let text = "-> Rows fetched before execution  (cost=0..0 rows=1)\n";
        let r = parse_tree_plan(text);
        assert_eq!(r[0].node_type, "Rows fetched before execution");
        assert_eq!(
            (r[0].startup_cost, r[0].total_cost, r[0].plan_rows),
            (Some(0.0), Some(0.0), Some(1.0))
        );
        let text = "-> Index range scan on t using idx over (5 < a)  (cost=1.2 rows=3)\n-> Limit: 10 row(s)\n    -> Select #2 (subquery in condition; run only once)  (never executed)\n";
        let r = parse_tree_plan(text);
        assert_eq!(r.len(), 2);
        assert_eq!(
            (
                r[0].node_type.as_str(),
                r[0].index.as_deref(),
                r[0].condition.as_deref()
            ),
            ("Index range scan", Some("idx"), Some("(5 < a)"))
        );
        assert_eq!(
            (r[1].node_type.as_str(), r[1].condition.as_deref()),
            ("Limit", Some("10 row(s)"))
        );
        assert_eq!(r[1].children[0].node_type, "Select #2");
        assert_eq!(r[1].children[0].loops, Some(0.0));
        let r =
            parse_tree_plan("-> Remove duplicates from input sorted on a_id  (cost=0.451 rows=2)");
        assert_eq!(
            r[0].node_type,
            "Remove duplicates from input sorted on a_id"
        );
        assert!(r[0].relation.is_none());
    }

    #[test]
    fn tabular() {
        let cols: Vec<String> = [
            "id",
            "select_type",
            "table",
            "type",
            "possible_keys",
            "key",
            "key_len",
            "ref",
            "rows",
            "Extra",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let rows = vec![vec![
            Some("1".into()),
            Some("SIMPLE".into()),
            Some("b".into()),
            Some("index".into()),
            Some("a_id".into()),
            Some("a_id".into()),
            Some("5".into()),
            None,
            Some("3".into()),
            Some("Using index".into()),
        ]];
        let n = tabular_nodes(&cols, &rows);
        assert_eq!(
            (
                n[0].node_type.as_str(),
                n[0].relation.as_deref(),
                n[0].plan_rows
            ),
            ("index", Some("b"), Some(3.0))
        );
        let t = tabular_text(&cols, &rows);
        assert!(t.starts_with("id | select_type | table | type"), "{t}");
        assert!(t.contains("NULL"));
    }
}
