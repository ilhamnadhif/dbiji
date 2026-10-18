//! SQL Server XML showplans (`SET SHOWPLAN_XML` / `SET STATISTICS XML`) →
//! [`PlanNode`] trees: one root per statement, with the `RelOp` operator
//! tree below it.

use dbiji_core::plan::{PlanNode, QueryPlan};
use dbiji_core::{DbError, DbResult};
use quick_xml::XmlVersion;
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;

fn attrs(e: &BytesStart<'_>) -> Vec<(String, String)> {
    e.attributes()
        .filter_map(|a| a.ok())
        .map(|a| {
            let key = a.key.local_name().into_inner().to_string();
            let value = a.normalized_value(XmlVersion::Implicit1_0).map(|v| v.into_owned()).unwrap_or_default();
            (key, value)
        })
        .collect()
}

fn get<'a>(a: &'a [(String, String)], key: &str) -> Option<&'a str> {
    a.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn num(a: &[(String, String)], key: &str) -> Option<f64> {
    get(a, key).and_then(|v| v.trim().parse().ok())
}

fn unbracket(s: &str) -> String {
    s.trim().trim_start_matches('[').trim_end_matches(']').replace("]]", "]")
}

/// Accumulated runtime counters of one operator.
#[derive(Default)]
struct Runtime {
    rows: f64,
    executions: f64,
    elapsed_ms: f64,
    cpu_ms: f64,
    seen: bool,
}

struct OpFrame {
    node: PlanNode,
    runtime: Runtime,
    depth: usize,
    has_object: bool,
}

struct StmtFrame {
    node: PlanNode,
    depth: usize,
}

const RELOP_PROPS: &[(&str, &str)] = &[
    ("LogicalOp", "Logical Operation"),
    ("EstimateIO", "Estimated I/O Cost"),
    ("EstimateCPU", "Estimated CPU Cost"),
    ("EstimateRebinds", "Estimated Rebinds"),
    ("EstimateRewinds", "Estimated Rewinds"),
    ("EstimatedRowsRead", "Estimated Rows to be Read"),
    ("TableCardinality", "Table Cardinality"),
    ("Parallel", "Parallel"),
    ("EstimatedExecutionMode", "Estimated Execution Mode"),
    ("NodeId", "Node ID"),
];

const STMT_PROPS: &[(&str, &str)] = &[
    ("StatementText", "Statement"),
    ("StatementOptmLevel", "Optimization Level"),
    ("StatementOptmEarlyAbortReason", "Early Abort Reason"),
    ("CardinalityEstimationModelVersion", "Cardinality Estimation Model"),
    ("QueryHash", "Query Hash"),
    ("QueryPlanHash", "Query Plan Hash"),
];

/// Parsed plans of one showplan document.
#[derive(Default)]
struct Parsed {
    roots: Vec<PlanNode>,
    compile_ms: f64,
    elapsed_ms: f64,
    has_time: bool,
}

fn parse_doc(xml: &str) -> DbResult<Parsed> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut out = Parsed::default();
    let mut path: Vec<String> = Vec::new();
    let mut ops: Vec<OpFrame> = Vec::new();
    let mut stmts: Vec<StmtFrame> = Vec::new();
    loop {
        let ev = reader
            .read_event()
            .map_err(|e| DbError::internal(format!("Invalid showplan XML: {e}")))?;
        let (start, empty) = match &ev {
            Event::Start(e) => (Some(e.clone()), false),
            Event::Empty(e) => (Some(e.clone()), true),
            _ => (None, false),
        };
        if let Some(e) = start {
            let name = e.local_name().into_inner().to_string();
            let a = attrs(&e);
            let depth = path.len();
            let parent = path.last().map(String::as_str).unwrap_or("");
            match name.as_str() {
                n if n.starts_with("Stmt") => {
                    let ty = get(&a, "StatementType").map(str::to_string).unwrap_or_else(|| n.to_string());
                    let mut node = PlanNode::new(ty);
                    node.total_cost = num(&a, "StatementSubTreeCost");
                    node.plan_rows = num(&a, "StatementEstRows");
                    for (k, label) in STMT_PROPS {
                        if let Some(v) = get(&a, k) {
                            node.properties.push((label.to_string(), v.trim().to_string()));
                        }
                    }
                    if empty {
                        attach_stmt(&mut stmts, &mut out.roots, node);
                    } else {
                        stmts.push(StmtFrame { node, depth });
                    }
                }
                "QueryPlan" => {
                    if let Some(c) = num(&a, "CompileTime") {
                        out.compile_ms += c;
                    }
                    if let Some(st) = stmts.last_mut() {
                        for (k, label) in [("CachedPlanSize", "Cached Plan Size (KB)"), ("CompileTime", "Compile Time (ms)"), ("CompileCPU", "Compile CPU (ms)"), ("DegreeOfParallelism", "Degree of Parallelism")] {
                            if let Some(v) = get(&a, k) {
                                st.node.properties.push((label.to_string(), v.to_string()));
                            }
                        }
                    }
                }
                "QueryTimeStats" => {
                    if let Some(t) = num(&a, "ElapsedTime") {
                        out.elapsed_ms += t;
                        out.has_time = true;
                    }
                    if let Some(st) = stmts.last_mut() {
                        if let Some(v) = get(&a, "ElapsedTime") {
                            st.node.properties.push(("Elapsed Time (ms)".into(), v.to_string()));
                        }
                        if let Some(v) = get(&a, "CpuTime") {
                            st.node.properties.push(("CPU Time (ms)".into(), v.to_string()));
                        }
                    }
                }
                "RelOp" => {
                    let ty = get(&a, "PhysicalOp").unwrap_or("Operator").to_string();
                    let mut node = PlanNode::new(ty);
                    node.plan_rows = num(&a, "EstimateRows");
                    node.total_cost = num(&a, "EstimatedTotalSubtreeCost");
                    node.plan_width = num(&a, "AvgRowSize");
                    for (k, label) in RELOP_PROPS {
                        if let Some(v) = get(&a, k) {
                            if *k == "LogicalOp" && Some(v) == get(&a, "PhysicalOp") {
                                continue;
                            }
                            node.properties.push((label.to_string(), v.to_string()));
                        }
                    }
                    let frame = OpFrame { node, runtime: Runtime::default(), depth, has_object: false };
                    if empty {
                        finish_op(frame, &mut ops, &mut stmts, &mut out.roots);
                    } else {
                        ops.push(frame);
                    }
                }
                "Object" => {
                    if let Some(op) = ops.last_mut()
                        && !op.has_object {
                            op.has_object = true;
                            let schema = get(&a, "Schema").map(unbracket);
                            let table = get(&a, "Table").map(unbracket);
                            if let Some(t) = table {
                                op.node.relation = Some(match schema {
                                    Some(s) if !s.is_empty() => format!("{s}.{t}"),
                                    _ => t,
                                });
                            }
                            if let Some(i) = get(&a, "Index") {
                                op.node.index = Some(unbracket(i));
                            }
                            if let Some(al) = get(&a, "Alias") {
                                op.node.alias = Some(unbracket(al));
                            }
                            if let Some(db) = get(&a, "Database") {
                                op.node.properties.push(("Database".into(), unbracket(db)));
                            }
                        }
                }
                "ScalarOperator" => {
                    if let (Some(op), Some(s)) = (ops.last_mut(), get(&a, "ScalarString")) {
                        if parent == "Predicate" && op.node.condition.is_none() {
                            op.node.condition = Some(s.to_string());
                        } else if matches!(parent, "RangeExpressions" | "Prefix" | "StartRange" | "EndRange") {
                            let label = "Seek Predicate".to_string();
                            if !op.node.properties.iter().any(|(k, v)| *k == label && v == s) {
                                op.node.properties.push((label, s.to_string()));
                            }
                        }
                    }
                }
                "RunTimeCountersPerThread" => {
                    if let Some(op) = ops.last_mut() {
                        let r = &mut op.runtime;
                        r.seen = true;
                        r.rows += num(&a, "ActualRows").unwrap_or(0.0);
                        r.executions += num(&a, "ActualExecutions").unwrap_or(0.0);
                        r.elapsed_ms = r.elapsed_ms.max(num(&a, "ActualElapsedms").unwrap_or(0.0));
                        r.cpu_ms = r.cpu_ms.max(num(&a, "ActualCPUms").unwrap_or(0.0));
                    }
                }
                "Warnings" | "SpillToTempDb" | "NoJoinPredicate" | "PlanAffectingConvert" | "MemoryGrantWarning" => {
                    if let Some(op) = ops.last_mut()
                        && (name != "Warnings" || !a.is_empty()) {
                            let detail = a.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", ");
                            op.node.properties.push(("Warning".into(), if detail.is_empty() { name.clone() } else { format!("{name} {detail}") }));
                        }
                }
                _ => {}
            }
            if !empty {
                path.push(name);
            }
            continue;
        }
        match ev {
            Event::End(e) => {
                let name = e.local_name().into_inner().to_string();
                path.pop();
                let depth = path.len();
                if name == "RelOp" {
                    if let Some(frame) = ops.pop() {
                        if frame.depth == depth {
                            finish_op(frame, &mut ops, &mut stmts, &mut out.roots);
                        } else {
                            ops.push(frame);
                        }
                    }
                } else if name.starts_with("Stmt")
                    && let Some(frame) = stmts.pop() {
                        if frame.depth == depth {
                            // operators left open belong to this statement
                            attach_stmt(&mut stmts, &mut out.roots, frame.node);
                        } else {
                            stmts.push(frame);
                        }
                    }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    while let Some(frame) = ops.pop() {
        finish_op(frame, &mut ops, &mut stmts, &mut out.roots);
    }
    while let Some(frame) = stmts.pop() {
        attach_stmt(&mut stmts, &mut out.roots, frame.node);
    }
    Ok(out)
}

fn finish_op(mut frame: OpFrame, ops: &mut [OpFrame], stmts: &mut [StmtFrame], roots: &mut Vec<PlanNode>) {
    if frame.runtime.seen {
        let r = &frame.runtime;
        frame.node.actual_rows = Some(r.rows);
        frame.node.loops = Some(r.executions.max(1.0));
        frame.node.actual_total_ms = Some(r.elapsed_ms);
        if r.cpu_ms > 0.0 {
            frame.node.properties.push(("Actual CPU (ms)".into(), format!("{}", r.cpu_ms)));
        }
        if r.executions > 0.0 {
            frame.node.properties.push(("Actual Executions".into(), format!("{}", r.executions)));
        }
    }
    // Per-execution times: dbiji multiplies actual_total_ms by loops, while
    // SQL Server reports the total over all executions.
    if let (Some(t), Some(l)) = (frame.node.actual_total_ms, frame.node.loops)
        && l > 0.0 {
            frame.node.actual_total_ms = Some(t / l);
        }
    if let Some(parent) = ops.last_mut() {
        parent.node.children.push(frame.node);
    } else if let Some(st) = stmts.last_mut() {
        st.node.children.push(frame.node);
    } else {
        roots.push(frame.node);
    }
}

fn attach_stmt(stmts: &mut [StmtFrame], roots: &mut Vec<PlanNode>, node: PlanNode) {
    match stmts.last_mut() {
        Some(parent) => parent.node.children.push(node),
        None => roots.push(node),
    }
}

/// Build a [`QueryPlan`] from one or more showplan XML documents.
pub fn build_plan(docs: &[String], analyzed: bool) -> DbResult<QueryPlan> {
    let mut plan = QueryPlan { analyzed, ..Default::default() };
    let mut compile = 0.0;
    let mut elapsed = 0.0;
    let mut has_time = false;
    for d in docs {
        let p = parse_doc(d)?;
        compile += p.compile_ms;
        elapsed += p.elapsed_ms;
        has_time |= p.has_time;
        plan.roots.extend(p.roots);
    }
    if compile > 0.0 {
        plan.planning_ms = Some(compile);
    }
    if analyzed && has_time {
        plan.execution_ms = Some(elapsed);
    }
    plan.raw = Some(docs.join("\n"));
    plan.text = plan.to_text_tree();
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ESTIMATED: &str = r#"<?xml version="1.0" encoding="utf-16"?>
<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan" Version="1.564" Build="16.0.4305.5">
 <BatchSequence><Batch><Statements>
  <StmtSimple StatementText="SELECT o.id FROM dbo.orders o JOIN dbo.customers c ON c.id = o.customer_id WHERE o.total &gt; 10" StatementId="1" StatementCompId="1" StatementType="SELECT" StatementSubTreeCost="0.0298" StatementEstRows="5" QueryHash="0xABC">
   <QueryPlan CachedPlanSize="24" CompileTime="3" CompileCPU="2">
    <RelOp NodeId="0" PhysicalOp="Hash Match" LogicalOp="Inner Join" EstimateRows="5" EstimateIO="0" EstimateCPU="0.017" AvgRowSize="11" EstimatedTotalSubtreeCost="0.0298" Parallel="0" EstimatedExecutionMode="Row">
     <OutputList/>
     <Hash>
      <RelOp NodeId="1" PhysicalOp="Clustered Index Scan" LogicalOp="Clustered Index Scan" EstimateRows="5" EstimateIO="0.003" EstimateCPU="0.0001" AvgRowSize="15" EstimatedTotalSubtreeCost="0.0032" TableCardinality="10" Parallel="0" EstimatedExecutionMode="Row">
       <IndexScan Ordered="0">
        <Object Database="[shop]" Schema="[dbo]" Table="[orders]" Index="[PK_orders]" Alias="[o]" IndexKind="Clustered"/>
        <Predicate><ScalarOperator ScalarString="[shop].[dbo].[orders].[total] as [o].[total]&gt;(10.00)"><Compare CompareOp="GT"/></ScalarOperator></Predicate>
       </IndexScan>
      </RelOp>
      <RelOp NodeId="2" PhysicalOp="Index Seek" LogicalOp="Index Seek" EstimateRows="1" EstimatedTotalSubtreeCost="0.0063" AvgRowSize="11">
       <IndexScan>
        <Object Database="[shop]" Schema="[dbo]" Table="[customers]" Index="[PK_customers]" Alias="[c]"/>
        <SeekPredicates><SeekPredicateNew><SeekKeys><Prefix ScanType="EQ"><RangeColumns/><RangeExpressions><ScalarOperator ScalarString="[shop].[dbo].[orders].[customer_id] as [o].[customer_id]"/></RangeExpressions></Prefix></SeekKeys></SeekPredicateNew></SeekPredicates>
       </IndexScan>
      </RelOp>
     </Hash>
    </RelOp>
   </QueryPlan>
  </StmtSimple>
  <StmtSimple StatementText="SET NOCOUNT ON" StatementType="SET ON/OFF"/>
 </Statements></Batch></BatchSequence>
</ShowPlanXML>"#;

    #[test]
    fn estimated_plan() {
        let plan = build_plan(&[ESTIMATED.to_string()], false).unwrap();
        assert_eq!(plan.roots.len(), 2);
        let stmt = &plan.roots[0];
        assert_eq!(stmt.node_type, "SELECT");
        assert_eq!(stmt.total_cost, Some(0.0298));
        assert!(stmt.properties.iter().any(|(k, v)| k == "Query Hash" && v == "0xABC"));
        let hash = &stmt.children[0];
        assert_eq!(hash.node_type, "Hash Match");
        assert!(hash.properties.iter().any(|(k, v)| k == "Logical Operation" && v == "Inner Join"));
        assert_eq!(hash.children.len(), 2);
        let scan = &hash.children[0];
        assert_eq!(scan.relation.as_deref(), Some("dbo.orders"));
        assert_eq!(scan.index.as_deref(), Some("PK_orders"));
        assert_eq!(scan.alias.as_deref(), Some("o"));
        assert_eq!(scan.condition.as_deref(), Some("[shop].[dbo].[orders].[total] as [o].[total]>(10.00)"));
        assert!(!scan.properties.iter().any(|(k, _)| k == "Logical Operation"));
        let seek = &hash.children[1];
        assert!(seek.properties.iter().any(|(k, v)| k == "Seek Predicate" && v.contains("customer_id")));
        assert_eq!(plan.roots[1].node_type, "SET ON/OFF");
        assert_eq!(plan.planning_ms, Some(3.0));
        assert!(plan.text.contains("Clustered Index Scan on dbo.orders o (PK_orders)"), "{}", plan.text);
        assert!(!plan.analyzed);
    }

    const ACTUAL: &str = r#"<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan"><BatchSequence><Batch><Statements>
<StmtSimple StatementType="SELECT" StatementSubTreeCost="0.5">
 <QueryPlan CompileTime="1"><QueryTimeStats CpuTime="4" ElapsedTime="12"/>
  <RelOp NodeId="0" PhysicalOp="Nested Loops" LogicalOp="Inner Join" EstimateRows="10" EstimatedTotalSubtreeCost="0.5">
   <RunTimeInformation><RunTimeCountersPerThread Thread="0" ActualRows="8" ActualExecutions="1" ActualElapsedms="10" ActualCPUms="3"/></RunTimeInformation>
   <NestedLoops>
    <RelOp NodeId="1" PhysicalOp="Table Scan" LogicalOp="Table Scan" EstimateRows="10" EstimatedTotalSubtreeCost="0.1">
     <RunTimeInformation><RunTimeCountersPerThread Thread="1" ActualRows="4" ActualExecutions="1" ActualElapsedms="2"/><RunTimeCountersPerThread Thread="2" ActualRows="5" ActualExecutions="1" ActualElapsedms="3"/></RunTimeInformation>
     <TableScan><Object Table="[t]"/></TableScan>
    </RelOp>
   </NestedLoops>
  </RelOp>
 </QueryPlan>
</StmtSimple></Statements></Batch></BatchSequence></ShowPlanXML>"#;

    #[test]
    fn actual_plan() {
        let plan = build_plan(&[ACTUAL.to_string()], true).unwrap();
        assert!(plan.analyzed);
        assert_eq!(plan.execution_ms, Some(12.0));
        let nl = &plan.roots[0].children[0];
        assert_eq!(nl.actual_rows, Some(8.0));
        assert_eq!(nl.actual_total_ms, Some(10.0));
        let scan = &nl.children[0];
        assert_eq!(scan.actual_rows, Some(9.0));
        assert_eq!(scan.loops, Some(2.0));
        assert_eq!(scan.actual_total_ms, Some(1.5));
        assert_eq!(scan.relation.as_deref(), Some("t"));
        assert!(plan.text.contains("actual time"), "{}", plan.text);
    }

    #[test]
    fn invalid_xml() {
        assert!(build_plan(&["<ShowPlanXML><a></b></ShowPlanXML>".to_string()], false).is_err());
    }
}
