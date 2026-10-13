//! Shared helpers for the #509 Examples 1–4 acceptance tests
//! (`planner_layering_example{1,2,3,4}.rs`). Specs:
//! `docs/design_docs/proposals/planner-layering-example{1,2,3,4}-acceptance.md`.
//!
//! [`run_stages`] runs the real stages through
//! `asap_devtools::planning_stages::run_planning_stages`, the pipeline
//! `stage_pipeline` also runs; the example workloads come from
//! `asap_devtools::planner_layering_examples`. The functions under "Pending adapters" read properties the IR
//! cannot express yet (window summaries, retention). They return today's
//! only possible answer; the implementer of each feature replaces the body
//! with a read of the new IR. Tests that depend on them are `#[ignore]`d
//! with the feature.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet, HashSet};

use asap_aware_mapping::stage2_physical_asap_optimization::PhysicalCandidate;
use asap_aware_mapping::stage3_plan_selection::Selection;
use asap_devtools::planning_stages::{lower_promql_roots, run_planning_stages};
use asap_types::ir::flat::{flatten, FlatDag, NodeId};
use asap_types::ir::physical_export::PhysicalASAPDAG;
use asap_types::ir::{ASAPOp, Operator, QueryRoot};
use asap_types::post_asap::{
    FieldDataType, Materialization, SketchAlgorithm, SketchParams, SketchStatistic,
};
use asap_types::workload::{PlanningWorkload, QueryRecurrence, RepeatedDemand};

pub use asap_devtools::planner_layering_examples::*;

pub type Payload = Operator<NodeId>;

/// Lower and plan a PromQL workload.
pub fn run_promql(workload: &PlanningWorkload) -> Run {
    run_stages(workload, lower_promql(workload))
}

/// Whether `form` answers a query window by merging several summaries.
pub fn needs_merge(form: WindowForm, query_window_ms: u64) -> bool {
    match form {
        WindowForm::None => false,
        WindowForm::Sliding { length_ms, .. } => length_ms < query_window_ms,
        WindowForm::Tumbling { .. } | WindowForm::ExponentialHistogram { .. } => true,
    }
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
    /// The choice of a per-computation candidate for every Pass 1 target.
    pub label: String,
    pub dag: FlatDag,
    pub query_roots: Vec<NodeId>,
    pub roots: Vec<QueryRoot>,
}

impl Logical {
    /// Whether two queries read a common node. Only Pass 2's
    /// identical-expression rule (#588) shares a node across queries.
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
    pub candidate: PhysicalCandidate,
}

impl Physical {
    pub fn dag(&self) -> &PhysicalASAPDAG {
        &self.candidate.dag
    }
}

#[derive(Debug, Clone)]
pub struct Run {
    pub stage0_query_language_frontends: Logical,
    pub logical: Vec<Logical>,
    pub physical: Vec<Physical>,
    /// Logical candidates physical ASAP-aware optimization cannot implement:
    /// (logical id, reason).
    pub pruned: Vec<(String, String)>,
    pub selection: Selection,
}

impl Run {
    pub fn invalid(&self) -> BTreeMap<&str, &str> {
        self.selection
            .rejected
            .iter()
            .filter(|r| !r.valid)
            .map(|r| (r.id.as_str(), r.reason.as_str()))
            .collect()
    }

    pub fn cost(&self, id: &str) -> Option<f64> {
        self.selection.costs.get(id).map(|c| c.total)
    }

    pub fn physical(&self, id: &str) -> &Physical {
        self.physical.iter().find(|p| p.id == id).expect("id")
    }

    pub fn physical_of<'a>(&'a self, logical: &'a Logical) -> impl Iterator<Item = &'a Physical> {
        self.physical
            .iter()
            .filter(move |p| p.from_logical == logical.id)
    }
}

fn logical(id: String, label: String, roots: Vec<QueryRoot>) -> Logical {
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
        label,
        dag,
        query_roots,
        roots,
    }
}

/// Every stage over `roots`, the lowered entries of `workload`, with every
/// logical candidate. Every candidate of the examples composes and
/// plan selection selects one. Recurrence and predictability (which
/// materialization needs, Example 4) are not passed to plan selection.
pub fn run_stages(workload: &PlanningWorkload, roots: Vec<QueryRoot>) -> Run {
    let run = run_planning_stages(workload, roots).expect("planning stages run");
    let logical_candidates = run
        .stage1_logical_asap_optimization
        .into_iter()
        .map(|c| {
            let roots = c
                .roots
                .unwrap_or_else(|e| panic!("{} ({}) composes: {e}", c.id, c.label));
            logical(c.id, c.label, roots)
        })
        .collect();
    let physical = run
        .stage2_physical_asap_optimization
        .into_iter()
        .map(|candidate| Physical {
            id: candidate.id.clone(),
            from_logical: candidate.from_logical.clone(),
            query_roots: candidate.dag.roots.clone(),
            candidate,
        })
        .collect();
    Run {
        stage0_query_language_frontends: logical(
            "stage0-query-language-frontends".into(),
            "query-language frontends".into(),
            run.stage0_query_language_frontends,
        ),
        logical: logical_candidates,
        physical,
        pruned: run
            .stage2_pruned
            .into_iter()
            .map(|p| (p.from_logical, p.reason))
            .collect(),
        selection: run.stage3_plan_selection.expect("plan selection selects"),
    }
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
pub fn readers(dag: &impl ExportedDag, query_roots: &[NodeId], node: NodeId) -> BTreeSet<usize> {
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
            } | ASAPOp::SummaryEstimate { .. }
                | ASAPOp::SummaryMerge { .. }
        )
    )
}

pub fn is_merge(payload: &Payload) -> bool {
    matches!(payload, Operator::ASAP(ASAPOp::SummaryMerge { .. }))
}

/// The sketch build nodes (`SummaryAgg` over a sketch family) of `dag`,
/// with their algorithm and parameters.
pub fn sketch_builds(dag: &impl ExportedDag) -> Vec<(NodeId, SketchAlgorithm, SketchParams)> {
    dag.node_ids()
        .into_iter()
        .filter_map(|id| match dag.payload(id) {
            Operator::ASAP(ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(kind, _),
                ..
            }) => Some((id, kind.algorithm().clone(), kind.params().clone())),
            _ => None,
        })
        .collect()
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

/// One query's per-computation choice: the sorted sketch algorithms built in
/// its closure, or `"exact"` when it builds none.
pub fn query_option(dag: &impl ExportedDag, query_roots: &[NodeId], query: usize) -> String {
    let reach = closure(dag, query_roots[query]);
    let mut names: Vec<_> = sketch_builds(dag)
        .into_iter()
        .filter(|(id, ..)| reach.contains(id))
        .map(|(_, algorithm, _)| format!("{algorithm:?}"))
        .collect();
    names.sort();
    if names.is_empty() {
        "exact".into()
    } else {
        names.join("+")
    }
}

/// Nodes reachable from more than one query root.
pub fn cross_query_nodes(dag: &impl ExportedDag, query_roots: &[NodeId]) -> BTreeSet<NodeId> {
    dag.node_ids()
        .into_iter()
        .filter(|&id| readers(dag, query_roots, id).len() > 1)
        .collect()
}

pub fn assert_valid_and_uniquely_named(run: &Run) {
    let ids: BTreeSet<_> = run.logical.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids.len(), run.logical.len(), "unique logical ids");
    for c in &run.logical {
        for root in &c.roots {
            root.validate_structure()
                .unwrap_or_else(|e| panic!("{}: {e}", c.id));
        }
    }
}

/// Physical ASAP-aware optimization maps the logical candidates one-to-one
/// onto physical ones, so no candidate is discarded before plan selection.
pub fn assert_one_physical_candidate_per_logical(run: &Run) {
    assert!(run.pruned.is_empty(), "pruned: {:?}", run.pruned);
    let sources: BTreeSet<_> = run
        .physical
        .iter()
        .map(|p| p.from_logical.as_str())
        .collect();
    let logical: BTreeSet<_> = run.logical.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(sources, logical);
    assert_eq!(run.physical.len(), run.logical.len());
    let ids: BTreeSet<_> = run.physical.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids.len(), run.physical.len(), "unique physical ids");
}

/// One selected id; every other candidate rejected once, with a reason.
pub fn assert_selects_one_and_explains_the_rest(run: &Run) {
    let all: BTreeSet<_> = run.physical.iter().map(|p| p.id.clone()).collect();
    let mut accounted: BTreeSet<_> = run
        .selection
        .rejected
        .iter()
        .map(|r| r.id.clone())
        .collect();
    assert_eq!(
        accounted.len(),
        run.selection.rejected.len(),
        "rejected once each"
    );
    assert!(run.selection.rejected.iter().all(|r| !r.reason.is_empty()));
    assert!(
        accounted.insert(run.selection.selected.clone()),
        "selected is not rejected"
    );
    assert_eq!(accounted, all);
}

/// The selected plan costs no more than any valid candidate.
pub fn assert_selects_cheapest_valid(run: &Run) {
    let invalid = run.invalid();
    let best = run
        .cost(&run.selection.selected)
        .expect("selected is priced");
    for p in run
        .physical
        .iter()
        .filter(|p| !invalid.contains_key(p.id.as_str()))
    {
        assert!(best <= run.cost(&p.id).unwrap(), "{} is cheaper", p.id);
    }
}

/// Each valid candidate's cost has one entry per DAG node, `total` is their
/// sum, and each node's multiplier is [`computations`]. Invalid candidates
/// are not priced.
pub fn assert_each_node_charged_per_consuming_query(run: &Run) {
    let invalid = run.invalid();
    for p in &run.physical {
        if invalid.contains_key(p.id.as_str()) {
            assert!(run.cost(&p.id).is_none(), "{} is priced", p.id);
            continue;
        }
        let cost = &run.selection.costs[&p.id];
        let nodes: BTreeSet<_> = p.dag().nodes.iter().map(|n| n.id).collect();
        let charged: BTreeSet<_> = cost.per_node.keys().copied().collect();
        assert_eq!(charged, nodes, "{}", p.id);
        let sum: f64 = cost.per_node.values().map(|c| c.cost).sum();
        assert!(
            (cost.total - sum).abs() <= 1e-9 * sum.abs().max(1.0),
            "{}",
            p.id
        );
        let times = computations(p.dag());
        for (id, c) in &cost.per_node {
            assert_eq!(c.multiplier, times[id], "{}: multiplier of {id:?}", p.id);
        }
    }
}

/// How many times each node is computed per workload evaluation (#509; the
/// rule of plan selection's private `computations`): 1 for a materialized
/// node; for a `NotMaterialized` one, the number of consumers that reach
/// it. A consumer is a query (operator or scalar root) or a materialized
/// node, whose walk starts at its inputs; a walk stops at materialized
/// nodes and counts each node once.
pub fn computations(dag: &PhysicalASAPDAG) -> BTreeMap<NodeId, u64> {
    let materialized: BTreeSet<_> = dag
        .nodes
        .iter()
        .filter(|n| n.materialization != Materialization::NotMaterialized)
        .map(|n| n.id)
        .collect();
    let starts = dag
        .roots
        .iter()
        .map(|&root| vec![root])
        .chain(
            dag.scalar_roots
                .iter()
                .map(|(_, expr)| expr.operator_refs().into_iter().copied().collect()),
        )
        .chain(materialized.iter().map(|&id| dag.producers(id)));
    let mut times: BTreeMap<_, _> = dag
        .nodes
        .iter()
        .map(|n| (n.id, u64::from(materialized.contains(&n.id))))
        .collect();
    for start in starts {
        let mut seen = HashSet::new();
        let mut stack = start;
        while let Some(id) = stack.pop() {
            if materialized.contains(&id) || !seen.insert(id) {
                continue;
            }
            *times.get_mut(&id).expect("node") += 1;
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

pub fn lookback_ms(workload: &PlanningWorkload, query: usize) -> u64 {
    let entry = workload.query_workload.entries().nth(query).expect("entry");
    let asap_types::workload::DurationMs(ms) = entry.time_selection.lookback.expect("lookback");
    ms
}

// ── Materialization ──────────────────────────────────────────────────────

/// A node's materialization option (#509 "Materialization"), without its
/// storage medium.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MaterializationOption {
    /// Runs as data arrives; output stored before any query asks.
    MatAtIngestion,
    /// Runs when a query first needs it; output kept for later executions
    /// and other queries of the batch.
    MatAtQuery,
    /// Runs at query time for each execution; output discarded.
    NotMaterialized,
}

/// The materialization option physical ASAP-aware optimization assigned to
/// `node` (#667).
pub fn materialization(p: &Physical, node: NodeId) -> MaterializationOption {
    let n = p.dag().nodes.iter().find(|n| n.id == node).expect("node");
    match n.materialization {
        Materialization::MatAtIngestion(_) => MaterializationOption::MatAtIngestion,
        Materialization::MatAtQuery(_) => MaterializationOption::MatAtQuery,
        Materialization::NotMaterialized => MaterializationOption::NotMaterialized,
    }
}

pub fn runs_at_ingestion(p: &Physical, node: NodeId) -> bool {
    let n = p.dag().nodes.iter().find(|n| n.id == node).expect("node");
    n.output_state.timing == asap_types::post_asap::ExecutionTiming::IngestionTime
}

/// Every node upstream of an ingestion-time node also runs at ingestion time.
pub fn assert_ingestion_upstream_is_ingestion(run: &Run) {
    for p in &run.physical {
        for n in &p.dag().nodes {
            if !runs_at_ingestion(p, n.id) {
                continue;
            }
            for up in closure(p.dag(), n.id) {
                assert!(
                    runs_at_ingestion(p, up),
                    "{}: {up:?} feeds ingestion-time {:?} at query time",
                    p.id,
                    n.id
                );
            }
        }
    }
}

// ── Pending adapters ─────────────────────────────────────────────────────

/// The window summary a summary build node maintains (#509 Pass 2,
/// window-composition rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WindowForm {
    /// Rebuilt from the query window's raw samples (no window summary).
    None,
    /// Windows of `length_ms` starting every `slide_ms`.
    Sliding { length_ms: u64, slide_ms: u64 },
    /// Back-to-back windows of `length_ms`.
    Tumbling { length_ms: u64 },
    /// EH buckets covering `horizon_ms` of history.
    ExponentialHistogram { horizon_ms: u64 },
}

/// Pending (needs Pass 2 window composition, #601/#628): the IR has no
/// window summary yet, so every build is rebuilt from its query window.
pub fn window_form(_dag: &impl ExportedDag, _build: NodeId) -> WindowForm {
    WindowForm::None
}

/// Pending (needs retention in the physical DAG): how long a materialized
/// node's output is kept, in event time. `None` until the IR records it.
pub fn retention_ms(_p: &Physical, _node: NodeId) -> Option<u64> {
    None
}
