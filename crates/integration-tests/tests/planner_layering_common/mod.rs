//! Shared helpers for the #509 Examples 1–4 acceptance tests
//! (`planner_layering_example{1,2,3,4}.rs`). Specs:
//! `docs/design_docs/proposals/planner-layering-example{1,2,3,4}-acceptance.md`.
//!
//! [`run_stages`] runs the real stages: logical ASAP-aware optimization
//! (Pass 1 and composition), physical ASAP-aware optimization and plan
//! selection. The functions under "Pending adapters" read properties the IR
//! cannot express yet (window summaries, retention). They return today's
//! only possible answer; the implementer of each feature replaces the body
//! with a read of the new IR. Tests that depend on them are `#[ignore]`d
//! with the feature.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet, HashSet};

use asap_aware_mapping::per_computation_candidates::{
    compose_logical_candidate, generate_per_computation_candidates,
};
use asap_aware_mapping::physical_asap_optimization::{optimize_physical_asap, PhysicalCandidate};
use asap_aware_mapping::plan_selection::{select_plan, Selection};
use asap_aware_mapping::PlanningModels;
use asap_types::ir::flat::{flatten, FlatDag, NodeId};
use asap_types::ir::physical_export::PhysicalASAPDAG;
use asap_types::ir::{ASAPOp, Operator, QueryRoot};
use asap_types::post_asap::{
    FieldDataType, Materialization, SketchAlgorithm, SketchParams, SketchStatistic,
};
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    AccuracyRequirement, BatchEntry, DataArrival, DataDistribution, DataWorkload, DurationMs,
    Evidence, EvidenceSource, LatencyRequirement, PlanningWorkload, Predictability, Query,
    QueryLanguage, QueryRecurrence, QueryRequirements, QueryTimeScope, QueryWorkload, Rate,
    RepeatedDemand, RepeatingEntry, RepetitionInterval, TimeSelection, TimestampMs,
};

pub type Payload = Operator<NodeId>;

pub fn declared<T>(value: T) -> Evidence<T> {
    Evidence {
        value: Some(value),
        source: EvidenceSource::Declared,
        ..Default::default()
    }
}

// ── Workloads shared by Examples 3 and 4 ────────────────────────────────

pub const MINUTE_MS: u64 = 60_000;
/// PromQL's `y` is 365 days.
pub const YEAR_MS: u64 = 365 * 24 * 60 * MINUTE_MS;
/// Pattern A's batch execution time T (2026-01-01T00:00:00Z).
pub const T_MS: u64 = 1_767_225_600_000;

/// The #509 shared data workload with `arrival`.
pub fn shared_data_workload(arrival: DataArrival) -> DataWorkload {
    DataWorkload {
        data_ingestion_interval: declared(DurationMs(15_000)),
        ingestion_volume: Evidence::default(),
        // Data at rest has no ingestion rate (`validate` rejects one).
        ingestion_rate: match arrival {
            DataArrival::AtRest => Evidence::default(),
            _ => declared(Rate(1_000_000.0 / 15.0)),
        },
        input_cardinality: declared(1_000_000),
        distribution: declared(DataDistribution::Zipf),
        arrival,
    }
}

/// Pattern A's five queries: (PromQL, `lookback`, `as_of` − T).
pub const PATTERN_A: [(&str, u64, u64); 5] = [
    ("quantile_over_time(0.99, latency_ms[5y])", 5 * YEAR_MS, 0),
    ("quantile_over_time(0.99, latency_ms[1y])", YEAR_MS, 0),
    (
        "quantile_over_time(0.99, latency_ms[1y] offset 1y)",
        YEAR_MS,
        YEAR_MS,
    ),
    (
        "quantile_over_time(0.99, latency_ms[1y] offset 2y)",
        YEAR_MS,
        2 * YEAR_MS,
    ),
    (
        "quantile_over_time(0.99, latency_ms[3y] offset 2y)",
        3 * YEAR_MS,
        2 * YEAR_MS,
    ),
];

/// How Pattern A's batch recurs (Example 4 varies it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternARecurrence {
    /// As given: one `ad_hoc` batch run once at T.
    OnceAdHoc,
    /// Example 4: repeated monthly and `Predictable { known_at }`; each run
    /// reads the intervals ending at its own evaluation time.
    MonthlyPredictable,
}

fn pattern_a_requirements() -> QueryRequirements {
    QueryRequirements {
        accuracy: AccuracyRequirement::Explicit(AccuracyTarget::EpsilonDelta {
            epsilon: 0.005,
            delta: 0.01,
        }),
        response_latency: LatencyRequirement::Unspecified,
    }
}

/// Example 3, Pattern A: p99 over five historical intervals.
pub fn pattern_a(recurrence: PatternARecurrence, arrival: DataArrival) -> PlanningWorkload {
    let (batch, repeating) = match recurrence {
        PatternARecurrence::OnceAdHoc => (
            Some(
                PATTERN_A
                    .iter()
                    .map(|&(query, lookback, before_t)| BatchEntry {
                        query: Query(query.into()),
                        requirements: pattern_a_requirements(),
                        predictability: Predictability::AdHoc,
                        invocations: 1,
                        execute_at: Some(TimestampMs(T_MS)),
                        time_selection: TimeSelection {
                            scope: QueryTimeScope::Longitudinal,
                            lookback: Some(DurationMs(lookback)),
                            as_of: Some(TimestampMs(T_MS - before_t)),
                        },
                    })
                    .collect(),
            ),
            None,
        ),
        PatternARecurrence::MonthlyPredictable => (
            None,
            Some(
                PATTERN_A
                    .iter()
                    .map(|&(query, lookback, _)| RepeatingEntry {
                        query: Query(query.into()),
                        demand: RepeatedDemand::FixedInterval(RepetitionInterval(
                            (30 * 24 * 60 * MINUTE_MS) as u32,
                        )),
                        requirements: pattern_a_requirements(),
                        predictability: Predictability::Predictable {
                            known_at: Some(TimestampMs(T_MS)),
                        },
                        time_selection: TimeSelection {
                            scope: QueryTimeScope::Longitudinal,
                            lookback: Some(DurationMs(lookback)),
                            as_of: None,
                        },
                    })
                    .collect(),
            ),
        ),
    };
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: batch,
            repeating_queries: repeating,
        },
        data_workload: Some(shared_data_workload(arrival)),
    }
}

pub const PATTERN_B: &str = "quantile_over_time(0.99, latency_ms[5m])";
pub const PATTERN_B_WINDOW_MS: u64 = 5 * MINUTE_MS;
pub const PATTERN_B_INTERVAL_MS: u64 = MINUTE_MS;

/// Example 3, Pattern B: a p99 panel over the last 5 min, every minute.
pub fn pattern_b() -> PlanningWorkload {
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(vec![RepeatingEntry {
                query: Query(PATTERN_B.into()),
                demand: RepeatedDemand::FixedInterval(RepetitionInterval(
                    PATTERN_B_INTERVAL_MS as u32,
                )),
                requirements: QueryRequirements {
                    accuracy: AccuracyRequirement::Explicit(AccuracyTarget::EpsilonDelta {
                        epsilon: 0.01,
                        delta: 0.01,
                    }),
                    response_latency: LatencyRequirement::ExplicitMaxMs(200.0),
                },
                predictability: Predictability::Predictable { known_at: None },
                time_selection: TimeSelection {
                    scope: QueryTimeScope::RealTime,
                    lookback: Some(DurationMs(PATTERN_B_WINDOW_MS)),
                    as_of: None,
                },
            }]),
        },
        data_workload: Some(shared_data_workload(DataArrival::ContinuouslyIngesting)),
    }
}

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

// ── Language-specific frontends ──────────────────────────────────────────

/// PromQL workload roots, each series' identity as a column (the row
/// representation per-series state needs at runtime).
pub fn lower_promql(workload: &PlanningWorkload) -> Vec<QueryRoot> {
    asap_frontend_promql::lower_promql_query_workload(workload, 0)
        .expect("workload lowers")
        .into_iter()
        .map(|root| match root {
            QueryRoot::Operator(node) => QueryRoot::Operator(
                asap_types::ir::schema_support::with_promql_series_identity(&node)
                    .expect("series identity"),
            ),
            scalar => scalar,
        })
        .collect()
}

/// The non-ASAP operator's kind or the aggregate's first measure as
/// `aggregate:<kind>`, for each node of one query's frontend DAG, sorted
/// (node ids are post-order; compare as a set of operations).
pub fn frontend_operations(root: &QueryRoot) -> Vec<String> {
    let dag = flatten(std::slice::from_ref(root)).0;
    let mut ops: Vec<String> = dag
        .nodes
        .iter()
        .map(|n| {
            let json = serde_json::to_value(&n.operator).expect("serializes");
            match json["NonASAP"]["Aggregate"]["measures"][0]["kind"].as_str() {
                Some(measure) => format!("aggregate:{measure}"),
                None => relational(&n.operator).unwrap_or_else(|| "?".into()),
            }
        })
        .collect();
    ops.sort();
    ops
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
    pub language_specific_frontends: Logical,
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
    for root in &roots {
        root.validate_structure().expect("valid DAG");
    }
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

/// Every choice of a per-computation candidate for every target, the last
/// target varying fastest.
fn choices(counts: &[usize]) -> Vec<Vec<usize>> {
    counts.iter().fold(vec![vec![]], |prefixes, &count| {
        prefixes
            .into_iter()
            .flat_map(|prefix| {
                (0..count).map(move |i| {
                    let mut choice = prefix.clone();
                    choice.push(i);
                    choice
                })
            })
            .collect()
    })
}

/// The frontends' DAG, then logical ASAP-aware optimization (Pass 1 and
/// composition; Pass 2 is not implemented), physical ASAP-aware
/// optimization and plan selection over `roots`, the lowered entries of
/// `workload`. Plan selection reads only the accuracy targets and the data
/// workload; recurrence and predictability (which materialization needs,
/// Example 4) are not passed.
pub fn run_stages(workload: &PlanningWorkload, roots: Vec<QueryRoot>) -> Run {
    let language_specific_frontends = logical("S0".into(), "frontends".into(), roots.clone());
    let inventory = generate_per_computation_candidates(roots.into_iter().enumerate().collect())
        .expect("Pass 1");
    let counts: Vec<_> = inventory
        .targets
        .iter()
        .map(|target| target.choices.candidates.len())
        .collect();
    let mut logical_candidates = Vec::new();
    let mut physical = Vec::new();
    let mut pruned = Vec::new();
    for (index, choice) in choices(&counts).into_iter().enumerate() {
        let id = format!("L{}", index + 1);
        let roots: Vec<_> = compose_logical_candidate(&inventory, &choice)
            .unwrap_or_else(|e| panic!("{id} {choice:?} composes: {e}"))
            .into_iter()
            .map(|(_, root)| root)
            .collect();
        match optimize_physical_asap(&id, &roots) {
            Ok(mut candidate) => {
                candidate.id = format!("P{}", index + 1);
                candidate.label = format!("{choice:?}");
                physical.push(Physical {
                    id: candidate.id.clone(),
                    from_logical: id.clone(),
                    query_roots: candidate.dag.roots.clone(),
                    candidate,
                });
            }
            Err(e) => pruned.push((id.clone(), e.to_string())),
        }
        logical_candidates.push(logical(id, format!("{choice:?}"), roots));
    }
    let targets: Vec<_> = workload
        .query_workload
        .entries()
        .map(|entry| Some(entry.requirements.accuracy.target()))
        .collect();
    let candidates: Vec<_> = physical.iter().map(|p| p.candidate.clone()).collect();
    let selection = select_plan(
        &candidates,
        &targets,
        workload.data_workload.as_ref().expect("data workload"),
        PlanningModels::builtin(),
    )
    .expect("plan selection selects");
    Run {
        language_specific_frontends,
        logical: logical_candidates,
        physical,
        pruned,
        selection,
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

/// Each valid candidate's cost has one entry per DAG node and `total` is
/// their sum. A materialized node is charged once; a `NotMaterialized` node
/// once per consuming query, since it is recomputed for each (#509). With no
/// materialized node in the DAG (every candidate until #604), a node's
/// multiplier is the number of queries that read it. Invalid candidates are
/// not priced.
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
        let none_materialized = nodes
            .iter()
            .all(|&n| materialization(p, n) == MaterializationOption::NotMaterialized);
        for (&id, c) in &cost.per_node {
            let expected = if materialization(p, id) != MaterializationOption::NotMaterialized {
                Some(1)
            } else if none_materialized {
                Some(readers(p.dag(), &p.query_roots, id).len() as u64)
            } else {
                None
            };
            match expected {
                Some(times) => assert_eq!(c.multiplier, times, "{}: {id:?}", p.id),
                None => assert!(c.multiplier >= 1, "{}: {id:?}", p.id),
            }
        }
    }
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
    let DurationMs(ms) = entry.time_selection.lookback.expect("lookback");
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
