//! Shared helpers for the #509 Examples 1–4 acceptance tests
//! (`planner_layering_example{1,2,3,4}.rs`). Specs:
//! `docs/design_docs/proposals/planner-layering-example{1,2,3,4}-acceptance.md`.
//!
//! [`run_stages`] runs the real stages through
//! `asap_devtools::planning_stages::run_planning_stages`, the pipeline
//! `asap_devtools` provides; the example workloads come from
//! `asap_devtools::planner_layering_examples`. The functions under "Pending adapters" read properties the IR
//! cannot express yet (sliding and EH window summaries) or derive what it
//! does not record (retention). They return today's
//! only possible answer; the implementer of each feature replaces the body
//! with a read of the new IR. Tests that depend on them are `#[ignore]`d
//! with the feature.
#![allow(dead_code, unused_imports)]

use std::collections::{BTreeMap, BTreeSet, HashSet};

use asap_cost::CostInputs;
use asap_devtools::planning_stages::{lower_promql_roots, run_planning_stages_over};
use asap_types::deployment_capabilities::DeploymentCapabilities;
use asap_types::ir::flat::{flatten, FlatDag, NodeId};
use asap_types::ir::physical_export::PhysicalASAPDAG;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::schema::{FieldDataType, SketchAlgorithm, SketchParams, SketchStatistic};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, QueryRoot};
use asap_types::physical::Lifetime;
use asap_types::physical::PhysicalWorkloadPlan;
use asap_types::workload::{PlanningWorkload, QueryRecurrence, RepeatedDemand};

pub use asap_devtools::planner_layering_examples::*;

pub type Payload = Operator<NodeId>;

/// Whether `form` answers a query window by merging several summaries.
pub fn needs_merge(form: WindowForm, query_window_ms: u64) -> bool {
    match form {
        WindowForm::None => false,
        WindowForm::Sliding { length_ms, .. } => length_ms < query_window_ms,
        WindowForm::Tumbling { .. } | WindowForm::ExponentialHistogram { .. } => true,
    }
}

/// `roots` with no node shared between queries: each query reads its own
/// copy of every node it reaches (sharing within one query is kept). Pass 1
/// composes a candidate from memo groups, so queries that read an identical
/// sub-DAG share its group's node; this is the fixtures' independent
/// variant of such a candidate.
#[allow(dead_code)]
pub fn unshare(roots: &[QueryRoot]) -> Vec<QueryRoot> {
    use asap_types::ir::OperatorNode;
    use std::collections::HashMap;
    use std::rc::Rc;
    fn copy(
        node: &Rc<OperatorNode>,
        copies: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
    ) -> Rc<OperatorNode> {
        if let Some(copied) = copies.get(&Rc::as_ptr(node)) {
            return Rc::clone(copied);
        }
        let copied = Rc::new(OperatorNode {
            operator: node.operator.map_children(|child| copy(child, copies)),
            ..(**node).clone()
        });
        copies.insert(Rc::as_ptr(node), Rc::clone(&copied));
        copied
    }
    roots
        .iter()
        .map(|root| match root {
            QueryRoot::Operator(node) => QueryRoot::Operator(copy(node, &mut HashMap::new())),
            QueryRoot::Scalar(scalar) => QueryRoot::Scalar(scalar.clone()),
        })
        .collect()
}

// ── Query-language frontends ──────────────────────────────────────────

/// PromQL workload roots, each series' identity as a column.
pub fn lower_promql(workload: &PlanningWorkload) -> Vec<QueryRoot> {
    lower_promql_roots(workload).expect("workload lowers")
}

/// The non-ASAP operator's kind or the aggregate's first measure as
/// `aggregate:<kind>`, for each node of one query's frontend DAG, sorted
/// (node ids are post-order; compare as a set of operations).
pub fn frontend_operations(root: &QueryRoot) -> Vec<String> {
    let dag = flatten(std::slice::from_ref(root)).0;
    let mut ops: Vec<String> = dag.nodes.iter().map(|n| operation(&n.operator)).collect();
    ops.sort();
    ops
}

/// The aggregate's first measure as `aggregate:<kind>`, else the operator's
/// kind in snake case.
pub fn operation(payload: &Payload) -> String {
    let json = serde_json::to_value(payload).expect("serializes");
    match json["NonASAP"]["Aggregate"]["measures"][0]["kind"].as_str() {
        Some(measure) => format!("aggregate:{measure}"),
        None => snake_case(payload.kind_name()),
    }
}

// ── Pipeline ─────────────────────────────────────────────────────────────

/// One logical candidate. `query_roots` has one root per workload entry, in
/// `QueryWorkload::entries()` order; `roots` are the in-memory roots.
#[derive(Debug, Clone)]
pub struct Logical {
    pub id: String,
    pub dag: FlatDag,
    pub query_roots: Vec<NodeId>,
    pub roots: Vec<QueryRoot>,
}

impl Logical {
    /// Whether two queries read a common node. Only Pass 2's
    /// identical-expression rule shares a node across queries.
    pub fn shared_input(&self) -> bool {
        !cross_query_nodes(&self.dag, &self.query_roots).is_empty()
    }
}

/// One physical candidate, derived from the logical candidate
/// `candidate.from_logical`.
#[derive(Debug, Clone)]
pub struct Physical {
    pub id: String,
    pub from_logical: String,
    pub query_roots: Vec<NodeId>,
    pub candidate: PhysicalWorkloadPlan,
}

impl Physical {
    pub fn dag(&self) -> &PhysicalASAPDAG {
        &self.candidate.dag
    }
}

fn logical(id: String, roots: Vec<QueryRoot>) -> Logical {
    let dag = flatten(&roots).0;
    let query_roots = dag
        .roots
        .iter()
        .map(|root| match root {
            QueryRoot::Operator(id) => *id,
            QueryRoot::Scalar(_) => panic!("the examples have operator roots"),
        })
        .collect();
    Logical {
        id,
        dag,
        query_roots,
        roots,
    }
}

fn logical_stage0(roots: Vec<QueryRoot>) -> Logical {
    logical("stage0-query-language-frontends".into(), roots)
}

// ── DAG helpers ──────────────────────────────────────────────────────────

/// The logical (flat) and physical DAGs share node ids and payloads; the
/// helpers below read only those and each node's producers and consumers.
pub trait ExportedDag {
    fn producers(&self, consumer: NodeId) -> Vec<NodeId>;
    fn consumers(&self, producer: NodeId) -> Vec<NodeId>;
    fn node_ids(&self) -> Vec<NodeId>;
    fn payload(&self, id: NodeId) -> &Payload;
}

impl ExportedDag for FlatDag {
    fn producers(&self, consumer: NodeId) -> Vec<NodeId> {
        self.nodes[consumer]
            .operator
            .children()
            .into_iter()
            .copied()
            .collect()
    }
    fn consumers(&self, producer: NodeId) -> Vec<NodeId> {
        self.node_ids()
            .into_iter()
            .filter(|&id| self.producers(id).contains(&producer))
            .collect()
    }
    fn node_ids(&self) -> Vec<NodeId> {
        (0..self.nodes.len()).collect()
    }
    fn payload(&self, id: NodeId) -> &Payload {
        &self.nodes[id].operator
    }
}

impl ExportedDag for PhysicalASAPDAG {
    fn producers(&self, consumer: NodeId) -> Vec<NodeId> {
        let edges = self.edges.iter().filter(|e| e.consumer == consumer);
        edges.map(|e| e.producer).collect()
    }
    fn consumers(&self, producer: NodeId) -> Vec<NodeId> {
        let edges = self.edges.iter().filter(|e| e.producer == producer);
        edges.map(|e| e.consumer).collect()
    }
    fn node_ids(&self) -> Vec<NodeId> {
        self.nodes.iter().map(|n| n.id).collect()
    }
    fn payload(&self, id: NodeId) -> &Payload {
        &self
            .nodes
            .iter()
            .find(|n| n.id == id)
            .expect("node")
            .payload
    }
}

/// Every node `root` depends on, including itself.
pub fn closure(dag: &impl ExportedDag, root: NodeId) -> HashSet<NodeId> {
    let mut seen = HashSet::from([root]);
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        for producer in dag.producers(node) {
            if seen.insert(producer) {
                stack.push(producer);
            }
        }
    }
    seen
}

/// The queries (indexes into `query_roots`) whose result depends on `node`.
pub fn queries(dag: &impl ExportedDag, query_roots: &[NodeId], node: NodeId) -> BTreeSet<usize> {
    query_roots
        .iter()
        .enumerate()
        .filter(|(_, &root)| closure(dag, root).contains(&node))
        .map(|(q, _)| q)
        .collect()
}

/// The non-ASAP operator's kind in snake case (`"scan"`, `"time_range"`, …).
pub fn relational(payload: &Payload) -> Option<String> {
    match payload {
        Operator::NonASAP(_) => Some(snake_case(payload.kind_name())),
        Operator::ASAP(_) => None,
    }
}

fn snake_case(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

pub fn is_summary(payload: &Payload) -> bool {
    matches!(
        payload,
        Operator::ASAP(
            ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(..),
                ..
            } | ASAPOp::SummaryTumblingWindowChain {
                family: FieldDataType::Sketch(..),
                ..
            } | ASAPOp::SummaryEstimate { .. }
                | ASAPOp::SummaryMerge { .. }
        )
    )
}

pub fn is_merge(payload: &Payload) -> bool {
    matches!(payload, Operator::ASAP(ASAPOp::SummaryMerge { .. }))
}

/// The estimation nodes that read `build`, directly or through merges.
pub fn estimates_of(dag: &impl ExportedDag, build: NodeId) -> Vec<(NodeId, SketchStatistic)> {
    let mut out = Vec::new();
    let mut stack = vec![build];
    let mut seen = HashSet::new();
    while let Some(node) = stack.pop() {
        for consumer in dag.consumers(node) {
            if !seen.insert(consumer) {
                continue;
            }
            match dag.payload(consumer) {
                Operator::ASAP(ASAPOp::SummaryEstimate { query, .. }) => {
                    out.push((consumer, query.clone()))
                }
                Operator::ASAP(ASAPOp::SummaryMerge { .. }) => stack.push(consumer),
                _ => {}
            }
        }
    }
    out
}

/// Nodes reachable from more than one query root.
pub fn cross_query_nodes(dag: &impl ExportedDag, query_roots: &[NodeId]) -> BTreeSet<NodeId> {
    dag.node_ids()
        .into_iter()
        .filter(|&id| queries(dag, query_roots, id).len() > 1)
        .collect()
}

/// How many times each node is computed per evaluation of its consumers (#509; the
/// rule of plan selection's private `computations`): 1 for a materialized
/// node; for a not-materialized one, the number of consumers that reach
/// it. A consumer is a query (operator or scalar root) or a materialized
/// node, whose walk starts at its inputs; a walk stops at materialized
/// nodes and counts each node once. The consumers materialized at ingestion time of one
/// refresh window share one ingestion run and count once (#1151).
pub fn computations(dag: &PhysicalASAPDAG) -> BTreeMap<NodeId, u64> {
    let materialized_nodes: BTreeSet<_> = dag
        .nodes
        .iter()
        .filter(|n| n.lifetime != asap_types::physical::Lifetime::None)
        .map(|n| n.id)
        .collect();
    // Each walk's start and, for a consumer refreshed at ingestion, its
    // refresh window.
    let starts = dag
        .roots
        .iter()
        .map(|&root| (vec![root], None))
        .chain(
            dag.scalar_roots
                .iter()
                .map(|(_, expr)| (expr.operator_refs().into_iter().copied().collect(), None)),
        )
        .chain(materialized_nodes.iter().map(|&id| {
            let at_ingestion = dag
                .nodes
                .iter()
                .find(|n| n.id == id)
                .unwrap()
                .output_state
                .timing
                == ExecutionTiming::IngestionTime;
            let window = at_ingestion.then(|| dag.refresh_window(id)).flatten();
            (dag.producers(id), window)
        }));
    let mut times: BTreeMap<_, _> = dag
        .nodes
        .iter()
        .map(|n| (n.id, u64::from(materialized_nodes.contains(&n.id))))
        .collect();
    // The (node, refresh window) pairs whose ingestion run is counted.
    let mut runs = HashSet::new();
    for (start, window) in starts {
        let mut seen = HashSet::new();
        let mut stack = start;
        while let Some(id) = stack.pop() {
            if materialized_nodes.contains(&id) || !seen.insert(id) {
                continue;
            }
            if window.is_none_or(|window| runs.insert((id, window))) {
                *times.get_mut(&id).expect("node") += 1;
            }
            stack.extend(dag.producers(id));
        }
    }
    times
}

/// The evaluation interval of entry `query`, for a fixed-interval repeating
/// entry.
pub fn evaluation_interval_ms(workload: &PlanningWorkload, query: usize) -> Option<u64> {
    match &workload.query_workload.entries().nth(query)?.recurrence {
        QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(i)) => Some(u64::from(i.0)),
        _ => None,
    }
}

// ── Materialized outputs ─────────────────────────────────────────────────────────

/// Whether a node's output is materialized and at which timing, without how long
/// (`OneEvaluation` or `AcrossEvaluations`) or its storage medium.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Materializing {
    /// Runs as data arrives; output materialized until a query reads it.
    MaterializedAtIngestion,
    /// Runs when a query first needs it; output reused by the other
    /// queries of the evaluation, or, as query-time `AcrossEvaluations`,
    /// retained for later evaluations.
    MaterializedAtQueryTime,
    /// Output discarded after its query (lifetime `None`).
    NotMaterialized,
}

/// Whether physical ASAP-aware optimization materializes `node`'s output, and at
/// which timing (#667).
pub fn materializing(p: &Physical, node: NodeId) -> Materializing {
    let n = p.dag().nodes.iter().find(|n| n.id == node).expect("node");
    match (n.output_state.timing, n.lifetime) {
        (_, Lifetime::None) => Materializing::NotMaterialized,
        (ExecutionTiming::IngestionTime, _) => Materializing::MaterializedAtIngestion,
        (ExecutionTiming::QueryTime, _) => Materializing::MaterializedAtQueryTime,
    }
}

pub fn runs_at_ingestion(p: &Physical, node: NodeId) -> bool {
    let n = p.dag().nodes.iter().find(|n| n.id == node).expect("node");
    n.output_state.timing == asap_types::ir::properties::ExecutionTiming::IngestionTime
}

// ── Pending adapters ─────────────────────────────────────────────────────

/// The window summary a summary build node maintains (#509 Pass 2,
/// window-composition rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WindowForm {
    /// Rebuilt from the query window's raw data samples (no window summary).
    None,
    /// Windows of `length_ms` starting every `slide_ms`.
    Sliding { length_ms: u64, slide_ms: u64 },
    /// Back-to-back windows of `length_ms`.
    Tumbling { length_ms: u64 },
    /// EH buckets covering `horizon_ms` of history.
    ExponentialHistogram { horizon_ms: u64 },
}

/// The window summary of a build: a `SummaryTumblingWindowChain` keeps back-to-back tumbling windows of
/// its tumbling-window width (tumbling); a `SummaryAgg` is rebuilt from its query
/// window. Sliding windows and Exponential Histograms are not planned yet
/// (#628).
pub fn window_form(dag: &impl ExportedDag, build: NodeId) -> WindowForm {
    match dag.payload(build) {
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            tumbling_window_width_ms,
            ..
        }) => WindowForm::Tumbling {
            length_ms: *tumbling_window_width_ms,
        },
        _ => WindowForm::None,
    }
}

/// How far back from an evaluation a materialized node's materialized output
/// reaches in event time, derived the way plan selection prices what it
/// keeps: a tumbling-window chain keeps its `N`
/// tumbling windows of width `w`, plus the one being built when maintained at
/// ingestion time; a whole-window build keeps its window, and the next one
/// being built at ingestion time; each plus the offset of the time shifts
/// below it. `None` for any other node.
pub fn retention_ms(p: &Physical, node: NodeId) -> Option<u64> {
    let dag = p.dag();
    let at_ingestion = runs_at_ingestion(p, node);
    let below = |id: NodeId| dag.producers(id).into_iter().next();
    let offset_below = |mut id: Option<NodeId>| {
        let mut offset = 0u64;
        while let Some(at) = id {
            match dag.payload(at) {
                Operator::NonASAP(NonASAPOp::TimeShift { shift, .. }) => {
                    offset += shift.offset_ms.unsigned_abs()
                }
                Operator::NonASAP(NonASAPOp::Scan { .. }) => break,
                _ => {}
            }
            id = below(at);
        }
        offset
    };
    match dag.payload(node) {
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            tumbling_window_width_ms,
            tumbling_window_count,
            ..
        }) => {
            let tumbling_windows = u64::from(*tumbling_window_count) + u64::from(at_ingestion);
            Some(tumbling_windows * tumbling_window_width_ms + offset_below(below(node)))
        }
        Operator::ASAP(ASAPOp::SummaryAgg { .. }) => {
            let child = below(node)?;
            let Operator::NonASAP(NonASAPOp::TimeRange { range, .. }) = dag.payload(child) else {
                return None;
            };
            let windows = 1 + u64::from(at_ingestion);
            Some(windows * range.as_millis() as u64 + offset_below(below(child)))
        }
        _ => None,
    }
}
