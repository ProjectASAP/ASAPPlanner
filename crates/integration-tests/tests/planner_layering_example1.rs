//! Acceptance tests for #509 "Example 1: Aggregation over dimensions", MVP scope.
//!
//! Spec: `docs/design_docs/proposals/planner-layering-example1-acceptance.md`.
//! Written by the test designer before the Phase C stage APIs exist: every
//! stage test is `#[ignore]`d and calls the `todo!()` stubs in [`stages`],
//! which the implementer replaces with the real entry points.
//!
//! MVP scope: Stage 1 = Pass 1 + the identical-expression rule only (no
//! window-composition variants); Stage 2 = physical operator implementation
//! only (no materialization). Expected counts are 1 → 6 → 6 → 1, a subset of
//! the doc's 1 → 54 → 156 → 1.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use asap_aware_mapping::PlanningModels;
use asap_types::ir::export::{LogicalASAPDAG, LogicalASAPNodeId, LogicalASAPOperatorPayload};
use asap_types::post_asap::sketch::{GroupingStrategy, HydraKind, SketchAlgorithm};
use asap_types::pre_asap::schema::FieldDataType;
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    AccuracyRequirement, DataArrival, DataDistribution, DataWorkload, DurationMs, Evidence,
    EvidenceSource, LatencyRequirement, PlanningWorkload, Predictability, Query, QueryLanguage,
    QueryRequirements, QueryTimeScope, QueryWorkload, Rate, RepeatedDemand, RepeatingEntry,
    RepetitionInterval, TimeSelection,
};

/// Hypothetical Phase C stage API. Replace each `todo!()` with the real call.
#[allow(dead_code, unused_variables)]
mod stages {
    use super::*;

    /// One whole-workload candidate. `LogicalASAPDAG::root` names one query,
    /// so `query_roots` carries one root per workload entry, in
    /// `QueryWorkload::entries()` order (`[q1, q2]`), until the export
    /// supports several roots.
    #[derive(Debug, Clone)]
    pub struct LogicalCandidate {
        pub id: String,
        pub label: String,
        pub dag: LogicalASAPDAG,
        pub query_roots: Vec<LogicalASAPNodeId>,
    }

    /// Placeholder for `asap_types::ir::export::PhysicalASAPDAG` (phase A).
    /// Its payload is an alias of the logical payload, so payload-kind checks
    /// carry over unchanged.
    pub type PhysicalASAPDAG = LogicalASAPDAG;

    /// One Stage 2 candidate, derived from exactly one Stage 1 candidate.
    #[derive(Debug, Clone)]
    pub struct PhysicalCandidate {
        pub id: String,
        pub from_logical: String,
        pub label: String,
        pub dag: PhysicalASAPDAG,
        pub query_roots: Vec<LogicalASAPNodeId>,
    }

    /// Whole-workload cost of one physical candidate; `per_node` has one
    /// entry per DAG node, so a shared node is charged once.
    #[derive(Debug, Clone)]
    pub struct CandidateCost {
        pub total: f64,
        pub per_node: BTreeMap<LogicalASAPNodeId, f64>,
    }

    /// A candidate Stage 3 did not select. `valid == false` means it failed
    /// an accuracy, latency or capability check; `true` means it lost on cost.
    #[derive(Debug, Clone)]
    pub struct Rejection {
        pub id: String,
        pub valid: bool,
        pub reason: String,
    }

    /// Stage 3 output. Costs are keyed by physical candidate id; the viewer
    /// document attaches them to the Stage 2 entries.
    #[derive(Debug, Clone)]
    pub struct Selection {
        pub selected: String,
        pub costs: BTreeMap<String, CandidateCost>,
        pub rejected: Vec<Rejection>,
    }

    /// Stage 0: frontends lower every query into one summary-free workload DAG.
    pub fn stage0_logical(workload: &PlanningWorkload) -> LogicalCandidate {
        todo!("Phase C: Stage 0 workload LogicalDAG")
    }

    /// Stage 1: Pass 1 local alternatives × Pass 2 identical-expression
    /// sharing. Takes the workload for accuracy requirements and time selection.
    pub fn stage1_logical_asap(
        workload: &PlanningWorkload,
        logical: &LogicalCandidate,
    ) -> Vec<LogicalCandidate> {
        todo!("Phase C: Stage 1 CandidateLogicalASAPDAGs")
    }

    /// Stage 2: physical operator implementation of every logical candidate
    /// (no materialization in the MVP).
    pub fn stage2_physical(
        workload: &PlanningWorkload,
        logical: &[LogicalCandidate],
    ) -> Vec<PhysicalCandidate> {
        todo!("Phase C: Stage 2 CandidatePhysicalASAPDAGs")
    }

    /// Stage 3: reject invalid candidates, cost the rest for the whole
    /// workload, select the cheapest.
    pub fn stage3_select(
        workload: &PlanningWorkload,
        physical: &[PhysicalCandidate],
        models: PlanningModels<'_>,
    ) -> Selection {
        todo!("Phase C: Stage 3 plan selection")
    }

    /// Stands in for `node.output_state.timing == IngestionTime` until
    /// `PhysicalASAPDAG` lands.
    pub fn runs_at_ingestion(candidate: &PhysicalCandidate, node: LogicalASAPNodeId) -> bool {
        todo!("Phase C: read PhysicalASAPDAGNode::output_state.timing")
    }
}

use stages::*;

// ── Example 1 workload ───────────────────────────────────────────────────

const Q1: &str = "sum by (job) (rate(http_requests_total[1m]))";
const Q2: &str = "topk by (job) (10, sum_over_time(http_requests_total[1m]))";

fn declared<T>(value: T) -> Evidence<T> {
    Evidence {
        value: Some(value),
        source: EvidenceSource::Declared,
        ..Default::default()
    }
}

fn dashboard_panel(query: &str, requirements: QueryRequirements) -> RepeatingEntry {
    RepeatingEntry {
        query: Query(query.into()),
        demand: RepeatedDemand::FixedInterval(RepetitionInterval(10_000)),
        requirements,
        predictability: Predictability::Predictable { known_at: None },
        time_selection: TimeSelection {
            scope: QueryTimeScope::RealTime,
            lookback: Some(DurationMs(60_000)),
            as_of: None,
        },
    }
}

/// Example 1 queries over the shared data workload of #509.
fn example1_workload() -> PlanningWorkload {
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(vec![
                dashboard_panel(
                    Q1,
                    QueryRequirements {
                        accuracy: AccuracyRequirement::Explicit(AccuracyTarget::Exact),
                        response_latency: LatencyRequirement::Unspecified,
                    },
                ),
                dashboard_panel(
                    Q2,
                    QueryRequirements {
                        accuracy: AccuracyRequirement::Explicit(AccuracyTarget::EpsilonDelta {
                            epsilon: 0.01,
                            delta: 0.001,
                        }),
                        response_latency: LatencyRequirement::ExplicitMaxMs(100.0),
                    },
                ),
            ]),
        },
        data_workload: Some(DataWorkload {
            arrival: DataArrival::ContinuouslyIngesting,
            data_ingestion_interval: declared(DurationMs(15_000)),
            ingestion_volume: Evidence::default(),
            ingestion_rate: declared(Rate(1_000_000.0 / 15.0)),
            input_cardinality: declared(1_000_000),
            distribution: declared(DataDistribution::Zipf),
        }),
    }
}

// ── DAG helpers ──────────────────────────────────────────────────────────

/// Q2's local option, read off its summary build node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Q2Option {
    Exact,
    CountMinHeapPerJob,
    Hydra,
}

/// Every node `root` depends on, including itself.
fn closure(dag: &LogicalASAPDAG, root: LogicalASAPNodeId) -> HashSet<LogicalASAPNodeId> {
    let mut seen = HashSet::from([root]);
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        for edge in dag.edges.iter().filter(|e| e.consumer == node) {
            if seen.insert(edge.producer) {
                stack.push(edge.producer);
            }
        }
    }
    seen
}

fn payload(dag: &LogicalASAPDAG, id: LogicalASAPNodeId) -> &LogicalASAPOperatorPayload {
    &dag.nodes.iter().find(|n| n.id == id).expect("node").payload
}

fn is_summary(payload: &LogicalASAPOperatorPayload) -> bool {
    matches!(
        payload,
        LogicalASAPOperatorPayload::SummaryAgg {
            family: FieldDataType::Sketch(..),
            ..
        } | LogicalASAPOperatorPayload::SummaryEstimate { .. }
            | LogicalASAPOperatorPayload::SummaryMerge
    )
}

/// Sketch families built in `nodes`, as Example 1's Q2 options.
fn sketch_options(dag: &LogicalASAPDAG, nodes: &HashSet<LogicalASAPNodeId>) -> BTreeSet<Q2Option> {
    nodes
        .iter()
        .filter_map(|&id| match payload(dag, id) {
            LogicalASAPOperatorPayload::SummaryAgg {
                family: FieldDataType::Sketch(kind, grouping),
                ..
            } => Some(match grouping {
                GroupingStrategy::SharedMultiSubpopulation {
                    kind: HydraKind::HydraCms,
                    ..
                } => Q2Option::Hydra,
                GroupingStrategy::PerSubpopulationInstance
                    if *kind.algorithm() == SketchAlgorithm::CmsWithHeap =>
                {
                    Q2Option::CountMinHeapPerJob
                }
                other => panic!("summary family outside Example 1: {kind:?} {other:?}"),
            }),
            _ => None,
        })
        .collect()
}

fn roots(query_roots: &[LogicalASAPNodeId]) -> (LogicalASAPNodeId, LogicalASAPNodeId) {
    assert_eq!(query_roots.len(), 2, "every candidate covers Q1 and Q2");
    (query_roots[0], query_roots[1])
}

/// Q2's option and whether Q1 and Q2 share any node, for one candidate.
fn classify(dag: &LogicalASAPDAG, query_roots: &[LogicalASAPNodeId]) -> (Q2Option, bool) {
    let (q1, q2) = roots(query_roots);
    let (c1, c2) = (closure(dag, q1), closure(dag, q2));
    let options = sketch_options(dag, &c2);
    assert!(options.len() <= 1, "Q2 uses one local option: {options:?}");
    let option = options.into_iter().next().unwrap_or(Q2Option::Exact);
    (option, !c1.is_disjoint(&c2))
}

/// The relational operator's wire `kind` (`"scan"`, `"sort"`, …); the
/// operator enum itself is not public outside `asap-types`.
fn relational(payload: &LogicalASAPOperatorPayload) -> Option<String> {
    match payload {
        LogicalASAPOperatorPayload::Relational { .. } => {
            let json = serde_json::to_value(payload).expect("payload serializes");
            json["operator"]["kind"].as_str().map(str::to_owned)
        }
        _ => None,
    }
}

fn pipeline() -> (
    PlanningWorkload,
    Vec<LogicalCandidate>,
    Vec<PhysicalCandidate>,
) {
    let workload = example1_workload();
    let logical = stage1_logical_asap(&workload, &stage0_logical(&workload));
    let physical = stage2_physical(&workload, &logical);
    (workload, logical, physical)
}

/// The six Example 1 MVP combinations: three Q2 options × separate/shared input.
fn expected_combinations() -> BTreeSet<(Q2Option, bool)> {
    [
        Q2Option::Exact,
        Q2Option::CountMinHeapPerJob,
        Q2Option::Hydra,
    ]
    .into_iter()
    .flat_map(|o| [(o, false), (o, true)])
    .collect()
}

// ── Workload ─────────────────────────────────────────────────────────────

/// The encoded workload is valid and normalizes to Q1 then Q2.
#[test]
fn workload_encodes_example1() {
    let workload = example1_workload();
    workload.validate().expect("valid workload");
    let entries: Vec<_> = workload.query_workload.entries().collect();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].query.0, Q1);
    assert_eq!(entries[1].query.0, Q2);
}

// ── Stage 0 ──────────────────────────────────────────────────────────────

/// Today's PromQL frontend already lowers each query to the doc's Stage 0 chain.
#[test]
fn stage0_frontend_lowers_each_query_to_doc_chain() {
    let roots = asap_frontend_promql::unified::lower_promql_query_workload(&example1_workload(), 0)
        .expect("Example 1 lowers");
    let chains: Vec<Vec<String>> = roots
        .iter()
        .map(|root| {
            let dag = asap_types::ir::export::compile_logical_asap_query(root).expect("compiles");
            let json = serde_json::to_value(&dag).expect("serializes");
            let mut ops: Vec<String> = json["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| {
                    let op = &n["payload"]["operator"];
                    match op["measures"][0]["kind"].as_str() {
                        Some(measure) => format!("aggregate:{measure}"),
                        None => op["kind"].as_str().unwrap_or("?").to_owned(),
                    }
                })
                .collect();
            ops.sort(); // node ids are assigned in post-order; compare as a set of operations
            ops
        })
        .collect();
    assert_eq!(
        chains,
        [
            ["aggregate:rate", "aggregate:sum", "scan", "time_range"],
            ["aggregate:sum", "aggregate:top_k", "scan", "time_range"],
        ]
    );
}

/// Stage 0 yields one workload DAG with one root per query and no summaries.
#[test]
#[ignore = "pending Phase C stage APIs"]
fn stage0_one_summary_free_workload_dag() {
    let stage0 = stage0_logical(&example1_workload());
    stage0.dag.validate().expect("valid DAG");
    roots(&stage0.query_roots);
    assert!(stage0.dag.nodes.iter().all(|n| !is_summary(&n.payload)));
}

/// Stage 0 keeps Q1 and Q2 separate; sharing is a Stage 1 decision.
#[test]
#[ignore = "pending Phase C stage APIs"]
fn stage0_queries_do_not_share_nodes() {
    let stage0 = stage0_logical(&example1_workload());
    let (q1, q2) = roots(&stage0.query_roots);
    assert!(closure(&stage0.dag, q1).is_disjoint(&closure(&stage0.dag, q2)));
}

// ── Stage 1 ──────────────────────────────────────────────────────────────

/// Stage 1 outputs exactly the 3 Q2 options × {separate, shared input} = 6 candidates.
#[test]
#[ignore = "pending Phase C stage APIs"]
fn stage1_has_six_candidates_covering_every_combination() {
    let (_, logical, _) = pipeline();
    assert_eq!(logical.len(), 6);
    let found: BTreeSet<_> = logical
        .iter()
        .map(|c| classify(&c.dag, &c.query_roots))
        .collect();
    assert_eq!(
        found,
        expected_combinations(),
        "each combination exactly once"
    );
}

/// Q1 is exact in every Stage 1 candidate: no summary is reachable from its root.
#[test]
#[ignore = "pending Phase C stage APIs"]
fn stage1_q1_is_always_exact() {
    let (_, logical, _) = pipeline();
    for c in &logical {
        let (q1, _) = roots(&c.query_roots);
        let reach = closure(&c.dag, q1);
        assert!(
            reach.iter().all(|&id| !is_summary(payload(&c.dag, id))),
            "{}: Q1 reaches a summary",
            c.id
        );
    }
}

/// Q2's summary families are exactly Count-Min + heap per job and Hydra over all jobs.
#[test]
#[ignore = "pending Phase C stage APIs"]
fn stage1_q2_summary_families_are_count_min_heap_and_hydra() {
    let (_, logical, _) = pipeline();
    let families: BTreeSet<_> = logical
        .iter()
        .flat_map(|c| {
            let (_, q2) = roots(&c.query_roots);
            sketch_options(&c.dag, &closure(&c.dag, q2))
        })
        .collect();
    assert_eq!(
        families,
        BTreeSet::from([Q2Option::CountMinHeapPerJob, Q2Option::Hydra])
    );
}

/// Sharing adds a variant and keeps the independent one, for every Q2 option.
#[test]
#[ignore = "pending Phase C stage APIs"]
fn stage1_keeps_independent_and_shared_variants() {
    let (_, logical, _) = pipeline();
    let found: Vec<_> = logical
        .iter()
        .map(|c| classify(&c.dag, &c.query_roots))
        .collect();
    for option in [
        Q2Option::Exact,
        Q2Option::CountMinHeapPerJob,
        Q2Option::Hydra,
    ] {
        assert!(
            found.contains(&(option, false)),
            "{option:?} independent missing"
        );
        assert!(found.contains(&(option, true)), "{option:?} shared missing");
    }
}

/// Only the raw input is shared between Q1 and Q2; no summary is shared.
#[test]
#[ignore = "pending Phase C stage APIs"]
fn stage1_shares_input_but_never_a_summary() {
    let (_, logical, _) = pipeline();
    for c in &logical {
        let (q1, q2) = roots(&c.query_roots);
        let shared = &closure(&c.dag, q1) & &closure(&c.dag, q2);
        for id in shared {
            let p = payload(&c.dag, id);
            assert!(
                matches!(relational(p).as_deref(), Some("scan" | "time_range")),
                "{}: shared node {id:?} is not the range selector input: {p:?}",
                c.id
            );
        }
    }
}

/// Stage 1 candidates are valid DAGs with unique ids.
#[test]
#[ignore = "pending Phase C stage APIs"]
fn stage1_candidates_are_valid_and_uniquely_named() {
    let (_, logical, _) = pipeline();
    let ids: BTreeSet<_> = logical.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids.len(), logical.len());
    for c in &logical {
        c.dag.validate().unwrap_or_else(|e| panic!("{}: {e}", c.id));
    }
}

// ── Stage 2 ──────────────────────────────────────────────────────────────

/// No candidate is discarded before Stage 3: Stage 2 maps the 6 logical candidates one-to-one.
#[test]
#[ignore = "pending Phase C stage APIs"]
fn stage2_keeps_every_logical_candidate() {
    let (_, logical, physical) = pipeline();
    assert_eq!(physical.len(), 6);
    let sources: BTreeSet<_> = physical.iter().map(|p| p.from_logical.as_str()).collect();
    let logical_ids: BTreeSet<_> = logical.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(sources, logical_ids);
    let ids: BTreeSet<_> = physical.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids.len(), physical.len());
}

/// Stage 2 preserves each logical candidate's Q2 option and input sharing.
#[test]
#[ignore = "pending Phase C stage APIs"]
fn stage2_preserves_logical_choices() {
    let (_, logical, physical) = pipeline();
    for p in &physical {
        let source = logical.iter().find(|c| c.id == p.from_logical).unwrap();
        assert_eq!(
            classify(&p.dag, &p.query_roots),
            classify(&source.dag, &source.query_roots),
            "{} vs {}",
            p.id,
            source.id
        );
    }
}

/// Exact TopK is implemented as a sort followed by a limit.
#[test]
#[ignore = "pending Phase C stage APIs"]
fn stage2_exact_topk_is_sort_then_limit() {
    let (_, _, physical) = pipeline();
    let exact: Vec<_> = physical
        .iter()
        .filter(|p| classify(&p.dag, &p.query_roots).0 == Q2Option::Exact)
        .collect();
    assert_eq!(exact.len(), 2);
    for p in exact {
        let sort_then_limit = p.dag.edges.iter().any(|e| {
            relational(payload(&p.dag, e.producer)).as_deref() == Some("sort")
                && relational(payload(&p.dag, e.consumer)).as_deref() == Some("limit")
        });
        assert!(sort_then_limit, "{}: no sort → limit", p.id);
    }
}

/// A summary Q2 is a build node feeding a top-10 estimation node, with no merge.
#[test]
#[ignore = "pending Phase C stage APIs"]
fn stage2_summary_topk_is_build_then_estimate() {
    let (_, _, physical) = pipeline();
    for p in &physical {
        if classify(&p.dag, &p.query_roots).0 == Q2Option::Exact {
            continue;
        }
        let kinds: Vec<_> = p.dag.nodes.iter().map(|n| &n.payload).collect();
        assert!(
            !kinds
                .iter()
                .any(|k| matches!(k, LogicalASAPOperatorPayload::SummaryMerge)),
            "{}: no window summaries in the MVP, so no merge",
            p.id
        );
        let build_to_estimate = p.dag.edges.iter().any(|e| {
            matches!(
                payload(&p.dag, e.producer),
                LogicalASAPOperatorPayload::SummaryAgg {
                    family: FieldDataType::Sketch(..),
                    ..
                }
            ) && matches!(
                payload(&p.dag, e.consumer),
                LogicalASAPOperatorPayload::SummaryEstimate { .. }
            )
        });
        assert!(build_to_estimate, "{}: no build → estimate", p.id);
    }
}

/// With no materialization in the MVP, every node runs at query time.
#[test]
#[ignore = "pending Phase C stage APIs"]
fn stage2_everything_runs_at_query_time() {
    let (_, _, physical) = pipeline();
    for p in &physical {
        for n in &p.dag.nodes {
            assert!(
                !runs_at_ingestion(p, n.id),
                "{}: {:?} at ingestion",
                p.id,
                n.id
            );
        }
    }
}

// ── Stage 3 ──────────────────────────────────────────────────────────────

/// Stage 3 selects one candidate and gives every other one a reason.
#[test]
#[ignore = "pending Phase C stage APIs"]
fn stage3_selects_one_and_explains_the_rest() {
    let (workload, _, physical) = pipeline();
    let selection = stage3_select(&workload, &physical, PlanningModels::builtin());
    let all: BTreeSet<_> = physical.iter().map(|p| p.id.clone()).collect();
    let mut accounted: BTreeSet<_> = selection.rejected.iter().map(|r| r.id.clone()).collect();
    assert_eq!(
        accounted.len(),
        selection.rejected.len(),
        "rejected once each"
    );
    assert!(selection.rejected.iter().all(|r| !r.reason.is_empty()));
    assert!(
        accounted.insert(selection.selected.clone()),
        "selected is not rejected"
    );
    assert_eq!(accounted, all);
}

/// The selected plan is the cheapest valid candidate for the whole workload.
#[test]
#[ignore = "pending Phase C stage APIs"]
fn stage3_selects_cheapest_valid() {
    let (workload, _, physical) = pipeline();
    let selection = stage3_select(&workload, &physical, PlanningModels::builtin());
    let invalid: BTreeSet<_> = selection
        .rejected
        .iter()
        .filter(|r| !r.valid)
        .map(|r| r.id.as_str())
        .collect();
    let best = selection.costs[&selection.selected].total;
    for p in physical.iter().filter(|p| !invalid.contains(p.id.as_str())) {
        assert!(best <= selection.costs[&p.id].total, "{} is cheaper", p.id);
    }
}

/// Every node is charged exactly once, so a shared input is costed once for both queries.
#[test]
#[ignore = "pending Phase C stage APIs"]
fn stage3_charges_each_node_once() {
    let (workload, _, physical) = pipeline();
    let selection = stage3_select(&workload, &physical, PlanningModels::builtin());
    for p in &physical {
        let cost = &selection.costs[&p.id];
        let nodes: BTreeSet<_> = p.dag.nodes.iter().map(|n| n.id).collect();
        let charged: BTreeSet<_> = cost.per_node.keys().copied().collect();
        assert_eq!(
            charged, nodes,
            "{}: per-node costs cover each node once",
            p.id
        );
        let sum: f64 = cost.per_node.values().sum();
        assert!(
            (cost.total - sum).abs() <= 1e-9 * sum.abs().max(1.0),
            "{}",
            p.id
        );
    }
}

/// Sharing the input never costs more than reading it separately.
#[test]
#[ignore = "pending Phase C stage APIs"]
fn stage3_shared_input_is_not_costlier() {
    let (workload, _, physical) = pipeline();
    let selection = stage3_select(&workload, &physical, PlanningModels::builtin());
    let by_combo: BTreeMap<_, _> = physical
        .iter()
        .map(|p| {
            (
                classify(&p.dag, &p.query_roots),
                selection.costs[&p.id].total,
            )
        })
        .collect();
    for option in [
        Q2Option::Exact,
        Q2Option::CountMinHeapPerJob,
        Q2Option::Hydra,
    ] {
        assert!(
            by_combo[&(option, true)] <= by_combo[&(option, false)],
            "{option:?}"
        );
    }
}
