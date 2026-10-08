//! Shared helpers for the #509 Examples 2–4 acceptance tests
//! (`planner_layering_example{2,3,4}.rs`). Specs:
//! `docs/design_docs/proposals/planner-layering-example{2,3,4}-acceptance.md`.
//!
//! [`run_stages`] runs the real library pipeline (`plan_stages`: Stage 1 →
//! 2 → 3). The functions under "Pending adapters" read properties the IR
//! cannot express yet (window summaries, materialization, retention). They
//! return today's only possible answer; the implementer of each feature
//! replaces the body with a read of the new IR, as Example 1's stubs were
//! replaced. Tests that depend on them are `#[ignore]`d with the feature.
#![allow(dead_code)]

use asap_types::ir::NonASAPOp;
use std::collections::{BTreeMap, BTreeSet, HashSet};

use asap_physical_optimizer::implementation::physical_candidates::PhysicalCandidate;
#[path = "../executor_models/mod.rs"]
mod executor_models;

use asap_plan_selection::{plan_stages, PlanningModels, Selection};
use asap_types::ir::flat::{flatten, FlatDag};
use asap_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPNodeId};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::schema::{FieldDataType, SketchAlgorithm, SketchParams, SketchStatistic};
use asap_types::ir::QueryRoot;
use asap_types::ir::{ASAPOp, Operator};
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    AccuracyRequirement, BatchEntry, DataArrival, DataDistribution, DataWorkload, DurationMs,
    Evidence, EvidenceSource, LatencyRequirement, PlanningWorkload, Predictability, Query,
    QueryLanguage, QueryRecurrence, QueryRequirements, QueryTimeScope, QueryWorkload, Rate,
    RepeatedDemand, RepeatingEntry, RepetitionInterval, RootDemand, TimeSelection, TimestampMs,
};
use executor_models::executor_models;

pub type Payload = Operator<PhysicalASAPNodeId>;

/// Every enumerated candidate is built and displayed; the largest example
/// (Example 3, Pattern A) has 486 today.
pub const MAX_CANDIDATES: usize = 4096;

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
        metric_types: Default::default(),
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

// ── Lowering ─────────────────────────────────────────────────────────────

/// PromQL workload roots, each series' identity as a column (the row
/// representation per-series state needs, as in Example 1).
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

/// The relational operator's wire `kind`, or the aggregate's first measure
/// as `aggregate:<kind>`, for each node of one query's Stage 0 DAG, sorted
/// (node ids are post-order; compare as a set of operations).
pub fn stage0_operations(root: &QueryRoot) -> Vec<String> {
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

/// One Stage 1 candidate. `query_roots` has one root per workload entry, in
/// `QueryWorkload::entries()` order.
#[derive(Debug, Clone)]
pub struct Logical {
    pub id: String,
    /// From Pass 2's identical-expression variant.
    pub shared_input: bool,
    pub dag: FlatDag,
    pub query_roots: Vec<PhysicalASAPNodeId>,
}

/// One Stage 2 candidate, derived from the Stage 1 candidate `from_logical`.
#[derive(Debug, Clone)]
pub struct Physical {
    pub id: String,
    pub from_logical: String,
    pub dag: PhysicalASAPDAG,
    pub query_roots: Vec<PhysicalASAPNodeId>,
    pub stage2: PhysicalCandidate,
}

#[derive(Debug, Clone)]
pub struct Run {
    pub stage0: Logical,
    pub logical: Vec<Logical>,
    pub physical: Vec<Physical>,
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

fn export(roots: &[QueryRoot]) -> (FlatDag, Vec<PhysicalASAPNodeId>) {
    for root in roots {
        root.validate_structure().expect("valid DAG");
    }
    let dag = flatten(roots).0;
    let query_roots = dag
        .roots
        .iter()
        .map(|root| match root {
            QueryRoot::Operator(id) => *id,
            QueryRoot::Scalar(_) => panic!("Examples 2–4 have operator roots"),
        })
        .collect();
    (dag, query_roots)
}

/// Stage 0 → 3 over `roots`, the lowered entries of `workload`, through the
/// library's `plan_stages`, with each entry's demand (accuracy, recurrence,
/// predictability).
pub fn run_stages(workload: &PlanningWorkload, roots: Vec<QueryRoot>) -> Run {
    run_stages_with(workload, roots, executor_models())
}

/// [`run_stages`] with the deployment inputs `models`.
pub fn run_stages_with(
    workload: &PlanningWorkload,
    roots: Vec<QueryRoot>,
    models: PlanningModels<'_>,
) -> Run {
    let (dag, query_roots) = export(&roots);
    let stage0 = Logical {
        id: "S0".into(),
        shared_input: false,
        dag,
        query_roots,
    };
    let demand: Vec<_> = workload
        .query_workload
        .entries()
        .map(|entry| RootDemand::from(&entry))
        .collect();
    let run = plan_stages(
        roots.into_iter().enumerate().collect(),
        &demand,
        workload.data_workload.as_ref().expect("data workload"),
        models,
        MAX_CANDIDATES,
    )
    .expect("plans");
    let enumeration = run.enumeration.expect("enumerated");
    assert_eq!(
        enumeration.candidates.len(),
        enumeration.combinations,
        "every candidate is displayed"
    );
    let mut logical = Vec::new();
    let mut physical = Vec::new();
    for c in enumeration.candidates {
        assert!(!c.physical.is_empty(), "every candidate builds");
        let roots: Vec<_> = c
            .logical
            .expect("composes")
            .into_iter()
            .map(|(_, r)| r)
            .collect();
        let (dag, query_roots) = export(&roots);
        logical.push(Logical {
            id: c.physical[0].from_logical.clone(),
            shared_input: c.sharing.merges_after_composition(),
            dag,
            query_roots,
        });
        for p in c.physical {
            physical.push(Physical {
                id: p.id.clone(),
                from_logical: p.from_logical.clone(),
                dag: p.dag.clone(),
                query_roots: p.dag.roots.clone(),
                stage2: p,
            });
        }
    }
    Run {
        stage0,
        logical,
        physical,
        selection: enumeration.selection,
    }
}

// ── DAG helpers ──────────────────────────────────────────────────────────

/// The logical (flat) and physical DAGs share node ids and payloads; the
/// helpers below read only those and each node's producers and consumers.
pub trait ExportedDag {
    fn producers(&self, consumer: PhysicalASAPNodeId) -> Vec<PhysicalASAPNodeId>;
    fn consumers(&self, producer: PhysicalASAPNodeId) -> Vec<PhysicalASAPNodeId>;
    fn node_ids(&self) -> Vec<PhysicalASAPNodeId>;
    fn payload(&self, id: PhysicalASAPNodeId) -> &Payload;
}

impl ExportedDag for FlatDag {
    fn producers(&self, consumer: PhysicalASAPNodeId) -> Vec<PhysicalASAPNodeId> {
        self.nodes[consumer]
            .operator
            .children()
            .into_iter()
            .copied()
            .collect()
    }
    fn consumers(&self, producer: PhysicalASAPNodeId) -> Vec<PhysicalASAPNodeId> {
        (0..self.nodes.len())
            .filter(|&id| self.producers(id).contains(&producer))
            .collect()
    }
    fn node_ids(&self) -> Vec<PhysicalASAPNodeId> {
        (0..self.nodes.len()).collect()
    }
    fn payload(&self, id: PhysicalASAPNodeId) -> &Payload {
        &self.nodes[id].operator
    }
}

impl ExportedDag for PhysicalASAPDAG {
    fn producers(&self, consumer: PhysicalASAPNodeId) -> Vec<PhysicalASAPNodeId> {
        let edges = self.edges.iter().filter(|e| e.consumer == consumer);
        edges.map(|e| e.producer).collect()
    }
    fn consumers(&self, producer: PhysicalASAPNodeId) -> Vec<PhysicalASAPNodeId> {
        let edges = self.edges.iter().filter(|e| e.producer == producer);
        edges.map(|e| e.consumer).collect()
    }
    fn node_ids(&self) -> Vec<PhysicalASAPNodeId> {
        self.nodes.iter().map(|n| n.id).collect()
    }
    fn payload(&self, id: PhysicalASAPNodeId) -> &Payload {
        &self
            .nodes
            .iter()
            .find(|n| n.id == id)
            .expect("node")
            .payload
    }
}

/// Every node `root` depends on, including itself.
pub fn closure(dag: &impl ExportedDag, root: PhysicalASAPNodeId) -> HashSet<PhysicalASAPNodeId> {
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
pub fn readers(
    dag: &impl ExportedDag,
    query_roots: &[PhysicalASAPNodeId],
    node: PhysicalASAPNodeId,
) -> BTreeSet<usize> {
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
        Operator::NonASAP(_) => {
            let mut out = String::new();
            for (i, c) in payload.kind_name().chars().enumerate() {
                if c.is_ascii_uppercase() && i > 0 {
                    out.push('_');
                }
                out.push(c.to_ascii_lowercase());
            }
            Some(out)
        }
        Operator::ASAP(_) => None,
    }
}

pub fn is_summary(payload: &Payload) -> bool {
    matches!(
        payload,
        Operator::ASAP(ASAPOp::SummaryAgg {
            family: FieldDataType::Sketch(..),
            ..
        }) | Operator::ASAP(ASAPOp::SummaryEstimate { .. })
            | Operator::ASAP(ASAPOp::SummaryMerge { .. })
    )
}

/// The sketch build nodes (`SummaryAgg` over a sketch family) of `dag`,
/// with their algorithm and parameters.
pub fn sketch_builds(
    dag: &impl ExportedDag,
) -> Vec<(PhysicalASAPNodeId, SketchAlgorithm, SketchParams)> {
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
pub fn estimates_of(
    dag: &impl ExportedDag,
    build: PhysicalASAPNodeId,
) -> Vec<(PhysicalASAPNodeId, SketchStatistic)> {
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

/// One query's local option: the sorted sketch algorithms built in its
/// closure, or `"exact"` when it builds none.
pub fn query_option(
    dag: &impl ExportedDag,
    query_roots: &[PhysicalASAPNodeId],
    query: usize,
) -> String {
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
pub fn cross_query_nodes(
    dag: &impl ExportedDag,
    query_roots: &[PhysicalASAPNodeId],
) -> BTreeSet<PhysicalASAPNodeId> {
    dag.node_ids()
        .into_iter()
        .filter(|&id| readers(dag, query_roots, id).len() > 1)
        .collect()
}

pub fn assert_valid_and_uniquely_named(run: &Run) {
    let ids: BTreeSet<_> = run.logical.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids.len(), run.logical.len(), "unique logical ids");
    for c in &run.logical {
        for (id, node) in c.dag.nodes.iter().enumerate() {
            assert!(
                node.operator
                    .children()
                    .into_iter()
                    .all(|child| *child < id),
                "{}: children come before their parents",
                c.id
            );
        }
    }
}

/// Stage 2 gives every logical candidate exactly one all-query-time
/// physical candidate, and possibly more that materialize summaries.
pub fn assert_stage2_bijection(run: &Run) {
    let sources: BTreeSet<_> = run
        .physical
        .iter()
        .map(|p| p.from_logical.as_str())
        .collect();
    let logical: BTreeSet<_> = run.logical.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(sources, logical);
    let query_time = run
        .physical
        .iter()
        .filter(|p| p.stage2.materialization.is_empty())
        .count();
    assert_eq!(query_time, run.logical.len());
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
    assert!(accounted.insert(run.selection.selected.clone()));
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

/// Each priced candidate's cost has one entry per DAG node and `total` is
/// their sum, so a node read by several queries is charged once.
pub fn assert_each_node_charged_once(run: &Run) {
    let invalid = run.invalid();
    for p in &run.physical {
        if invalid.contains_key(p.id.as_str()) {
            assert!(run.cost(&p.id).is_none(), "{} is priced", p.id);
            continue;
        }
        let cost = &run.selection.costs[&p.id];
        let nodes: BTreeSet<_> = p.dag.nodes.iter().map(|n| n.id).collect();
        let charged: BTreeSet<_> = cost.per_node.keys().copied().collect();
        assert_eq!(charged, nodes, "{}", p.id);
        let sum: f64 = cost.per_node.values().map(|c| c.cost).sum();
        assert!(
            (cost.total - sum).abs() <= 1e-9 * sum.abs().max(1.0),
            "{}",
            p.id
        );
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

/// A build merged by a `SummaryMerge` is a tumbling pane, of the width its
/// `TimeRange` input reads (#580: pane `i` is `TimeRange(w)` over
/// `TimeShift(i·w)` over the scan); any other build is rebuilt from its
/// query window. Sliding windows and Exponential Histograms are not planned
/// yet.
pub fn window_form(dag: &impl ExportedDag, build: PhysicalASAPNodeId) -> WindowForm {
    let merged = dag
        .consumers(build)
        .into_iter()
        .any(|c| matches!(dag.payload(c), Operator::ASAP(ASAPOp::SummaryMerge { .. })));
    if !merged {
        return WindowForm::None;
    }
    let ranges: Vec<u64> = dag
        .producers(build)
        .into_iter()
        .filter_map(|input| match dag.payload(input) {
            Operator::NonASAP(NonASAPOp::TimeRange { range, .. }) => Some(range.as_millis() as u64),
            _ => None,
        })
        .collect();
    match ranges.as_slice() {
        [length_ms] => WindowForm::Tumbling {
            length_ms: *length_ms,
        },
        other => panic!("a pane reads one time range: {other:?}"),
    }
}

/// Stage 2's materialization choice for one node (#509 "Materialization").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Materialization {
    /// Runs as data arrives; output stored before any query asks.
    IngestionTime,
    /// Runs when a query first needs it; output kept for later executions
    /// and other queries of the batch.
    QueryTimeKept,
    /// Runs at query time for each execution; output discarded.
    NotMaterialized,
}

/// Stage 2 runs a node at ingestion time or at query time, recomputed at
/// each evaluation; it does not keep query-time output yet (Example 4 B3),
/// so `QueryTimeKept` does not occur.
pub fn materialization(p: &Physical, node: PhysicalASAPNodeId) -> Materialization {
    let n = p.dag.nodes.iter().find(|n| n.id == node).expect("node");
    match n.output_state.timing {
        ExecutionTiming::IngestionTime => Materialization::IngestionTime,
        ExecutionTiming::QueryTime => Materialization::NotMaterialized,
    }
}

/// How long a materialized node's output is kept, in event time. The export
/// records no retention, so this derives it as Stage 3 prices it
/// (`stage3-cost-model.md`): ingestion-time work read at query time through
/// a merge of `N` panes of width `w` keeps `(N + 1) · w`; read directly, the
/// window being built and the completed one, `2 · window`. Taken over every
/// query-time reader the node's ingestion-time work feeds. `None` for a
/// query-time node.
pub fn retention_ms(p: &Physical, node: PhysicalASAPNodeId) -> Option<u64> {
    if !runs_at_ingestion(p, node) {
        return None;
    }
    // The longest raw range an ingestion-time node reads.
    let window = |id: PhysicalASAPNodeId| {
        closure(&p.dag, id)
            .into_iter()
            .filter_map(|n| match p.dag.payload(n) {
                Payload::NonASAP(NonASAPOp::TimeRange { range, .. }) => {
                    Some(range.as_millis() as u64)
                }
                _ => None,
            })
            .max()
            .unwrap_or(0)
    };
    let mut kept = 0;
    let mut stack = vec![node];
    let mut seen = HashSet::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        for consumer in p.dag.consumers(id) {
            if runs_at_ingestion(p, consumer) {
                stack.push(consumer);
            } else if matches!(
                p.dag.payload(consumer),
                Operator::ASAP(ASAPOp::SummaryMerge { .. })
            ) {
                let panes = p.dag.producers(consumer).len() as u64;
                kept = kept.max((panes + 1) * window(id));
            } else {
                kept = kept.max(2 * window(id));
            }
        }
    }
    Some(kept)
}

pub fn runs_at_ingestion(p: &Physical, node: PhysicalASAPNodeId) -> bool {
    materialization(p, node) == Materialization::IngestionTime
}

/// Every node upstream of an ingestion-time node also runs at ingestion time.
pub fn assert_ingestion_upstream_is_ingestion(run: &Run) {
    for p in &run.physical {
        for n in &p.dag.nodes {
            if !runs_at_ingestion(p, n.id) {
                continue;
            }
            for up in closure(&p.dag, n.id) {
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
