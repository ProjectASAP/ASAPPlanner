//! Acceptance tests for #509 "Example 1: Aggregation over dimensions", MVP scope.
//!
//! Spec: `docs/design_docs/proposals/planner-layering-example1-acceptance.md`.
//! Written by the test designer before the Phase C stage APIs existed; the
//! implementer replaced the stubs in [`stages`] with adapters over the real
//! stages. Tests that still fail because the implementation differs from the
//! spec stay `#[ignore]`d with the difference as the reason.
//!
//! MVP scope: Stage 1 = Pass 1 + the identical-expression rule + the
//! window-composition rule's tumbling windows; Stage 2 = physical operator
//! implementation and materialization (ingestion time or query time per
//! summary). Counts follow the planner's output (user decision):
//! 1 → 88 → 112 → 1, because Pass 1 also offers exact accumulators and
//! whole-expression top-k sketches, Q2's exact sum also comes in 10-s
//! tumbling panes (44 combinations; rates and top-k heaps do not merge,
//! #580), Pass 2 adds a shared-input variant of each, and Stage 2 adds a
//! maintained-pane candidate for each of the 24 with panes. The doc's
//! 1 → 54 → 156 → 1 needs sliding windows and Hydra.

use std::collections::{BTreeMap, BTreeSet, HashSet};

mod executor_models;
use executor_models::executor_models;

use asap_plan_selection::PlanningModels;
use asap_types::ir::flat::{flatten, FlatDag, NodeId};
use asap_types::ir::schema::state_type::{GroupingStrategy, HydraKind, SketchAlgorithm};
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, Operator};
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    AccuracyRequirement, DataArrival, DataDistribution, DataWorkload, DurationMs, Evidence,
    EvidenceSource, LatencyRequirement, MetricType, PlanningWorkload, Predictability, Query,
    QueryLanguage, QueryRequirements, QueryTimeScope, QueryWorkload, Rate, RepeatedDemand,
    RepeatingEntry, RepetitionInterval, TimeSelection,
};

type Payload = Operator<NodeId>;

/// Adapters from the Phase C stage APIs to the shapes these tests were written
/// against. They only convert; every decision is the real stage's.
#[allow(dead_code)]
mod stages {
    use std::rc::Rc;

    use super::*;
    use asap_plan_selection::plan_stages;
    use asap_types::ir::{OperatorNode, QueryRoot};

    /// Every Example 1 candidate is built (88 with Q2's tumbling sums),
    /// above the selection fallback's 64.
    const DISPLAYED: usize = 128;

    /// One whole-workload candidate. `query_roots` holds one root per
    /// workload entry, in `QueryWorkload::entries()` order (`[q1, q2]`).
    /// `roots` are the in-memory roots the next stage consumes.
    #[derive(Debug, Clone)]
    pub struct LogicalCandidate {
        pub id: String,
        pub label: String,
        pub dag: FlatDag,
        pub query_roots: Vec<NodeId>,
        pub roots: Vec<Rc<OperatorNode>>,
    }

    pub type PhysicalASAPDAG = asap_types::ir::physical_export::PhysicalASAPDAG;

    /// One Stage 2 candidate, derived from exactly one Stage 1 candidate.
    #[derive(Debug, Clone)]
    pub struct PhysicalCandidate {
        pub id: String,
        pub from_logical: String,
        pub label: String,
        pub dag: PhysicalASAPDAG,
        pub query_roots: Vec<NodeId>,
        pub stage2: asap_physical_optimizer::implementation::physical_candidates::PhysicalCandidate,
    }

    /// Whole-workload cost of one physical candidate; `per_node` has one
    /// entry per DAG node, so a shared node is charged once.
    #[derive(Debug, Clone)]
    pub struct CandidateCost {
        pub total: f64,
        pub per_node: BTreeMap<NodeId, f64>,
    }

    /// A candidate Stage 3 did not select. `valid == false` means it failed
    /// an accuracy, latency or capability check; `true` means it lost on cost.
    #[derive(Debug, Clone)]
    pub struct Rejection {
        pub id: String,
        pub valid: bool,
        pub reason: String,
    }

    /// Stage 3 output. Costs are keyed by physical candidate id.
    #[derive(Debug, Clone)]
    pub struct Selection {
        pub selected: String,
        pub costs: BTreeMap<String, CandidateCost>,
        pub rejected: Vec<Rejection>,
    }

    /// The frontend DAG with each PromQL series' full identity as a column,
    /// the row representation per-series state needs at runtime.
    fn lower(workload: &PlanningWorkload) -> Vec<QueryRoot> {
        asap_frontend_promql::lower_promql_query_workload(workload, 0)
            .expect("Example 1 lowers")
            .into_iter()
            .map(|root| match root {
                QueryRoot::Operator(node) => QueryRoot::Operator(
                    asap_types::ir::schema_support::with_promql_series_identity(&node)
                        .expect("series identity"),
                ),
                QueryRoot::Scalar(_) => panic!("Example 1 has operator roots"),
            })
            .collect()
    }

    fn candidate(id: String, label: String, roots: Vec<QueryRoot>) -> LogicalCandidate {
        for root in &roots {
            root.validate_structure().expect("valid DAG");
        }
        let dag = flatten(&roots).0;
        let query_roots = dag
            .roots
            .iter()
            .map(|root| match root {
                QueryRoot::Operator(id) => *id,
                QueryRoot::Scalar(_) => panic!("Example 1 has operator roots"),
            })
            .collect();
        let roots = roots
            .into_iter()
            .map(|root| match root {
                QueryRoot::Operator(node) => node,
                QueryRoot::Scalar(_) => panic!("Example 1 has operator roots"),
            })
            .collect();
        LogicalCandidate {
            id,
            label,
            dag,
            query_roots,
            roots,
        }
    }

    /// Stage 0: frontends lower every query into one summary-free workload DAG.
    pub fn stage0_logical(workload: &PlanningWorkload) -> LogicalCandidate {
        candidate("S0".into(), "frontend".into(), lower(workload))
    }

    /// Stage 1: every combination of Pass 1 local alternatives, independent
    /// and with the shared input (Pass 2), as the library's stage pipeline
    /// enumerates them. Lowers `workload` again: Pass 1 reads the in-memory
    /// DAG, not the Stage 0 export.
    pub fn stage1_logical_asap(
        workload: &PlanningWorkload,
        _logical: &LogicalCandidate,
    ) -> Vec<LogicalCandidate> {
        let demand: Vec<_> = workload
            .query_workload
            .entries()
            .map(|entry| asap_types::workload::RootDemand::from(&entry))
            .collect();
        let run = plan_stages(
            lower(workload).into_iter().enumerate().collect(),
            &demand,
            workload.data_workload.as_ref().expect("data workload"),
            executor_models(),
            DISPLAYED,
        )
        .expect("plans");
        let enumeration = run.enumeration.expect("enumerated");
        assert_eq!(enumeration.candidates.len(), enumeration.combinations);
        enumeration
            .candidates
            .into_iter()
            .map(|c| {
                let physical = c
                    .physical
                    .into_iter()
                    .next()
                    .expect("every Example 1 candidate builds");
                let label = format!(
                    "{:?}{}",
                    c.choice,
                    if c.sharing.merges_after_composition() {
                        " shared"
                    } else {
                        ""
                    }
                );
                let roots = c.logical.expect("composes");
                candidate(
                    physical.from_logical,
                    label,
                    roots.into_iter().map(|(_, root)| root).collect(),
                )
            })
            .collect()
    }

    /// Stage 2: physical operator implementation of every logical candidate,
    /// and one candidate per materialization choice (all query time first,
    /// `P<n>`; then `P<n>-m<k>`).
    pub fn stage2_physical(
        workload: &PlanningWorkload,
        logical: &[LogicalCandidate],
    ) -> Vec<PhysicalCandidate> {
        let demand: Vec<_> = workload
            .query_workload
            .entries()
            .map(|entry| asap_types::workload::RootDemand::from(&entry))
            .collect();
        let data = workload.data_workload.as_ref().expect("data workload");
        logical
            .iter()
            .enumerate()
            .flat_map(|(index, l)| {
                let stage2 =
                    asap_physical_optimizer::implementation::physical_candidates::stage2_physical(
                        &l.id,
                        &l.roots,
                        &demand,
                        data,
                        &|_| None,
                    )
                    .unwrap_or_else(|e| panic!("{}: {e}", l.id));
                assert!(stage2.exhaustive, "{}", l.id);
                stage2
                    .candidates
                    .into_iter()
                    .enumerate()
                    .map(move |(k, mut stage2)| {
                        stage2.id = match k {
                            0 => format!("P{}", index + 1),
                            k => format!("P{}-m{k}", index + 1),
                        };
                        stage2.label = l.label.clone();
                        PhysicalCandidate {
                            id: stage2.id.clone(),
                            from_logical: stage2.from_logical.clone(),
                            label: stage2.label.clone(),
                            dag: stage2.dag.clone(),
                            query_roots: stage2.dag.roots.clone(),
                            stage2,
                        }
                    })
            })
            .collect()
    }

    /// Stage 3: reject invalid candidates, cost the rest for the whole
    /// workload, select the cheapest.
    pub fn stage3_select(
        workload: &PlanningWorkload,
        physical: &[PhysicalCandidate],
        models: PlanningModels<'_>,
    ) -> Selection {
        let demand: Vec<_> = workload
            .query_workload
            .entries()
            .map(|entry| asap_types::workload::RootDemand::from(&entry))
            .collect();
        let candidates: Vec<_> = physical.iter().map(|p| p.stage2.clone()).collect();
        let selection = asap_plan_selection::stage3_select(
            &candidates,
            &demand,
            workload.data_workload.as_ref().expect("data workload"),
            models,
        )
        .expect("Stage 3 selects");
        Selection {
            selected: selection.selected,
            costs: selection
                .costs
                .into_iter()
                .map(|(id, cost)| {
                    let per_node = cost
                        .per_node
                        .into_iter()
                        .map(|(node, c)| (node, c.cost))
                        .collect();
                    (
                        id,
                        CandidateCost {
                            total: cost.total,
                            per_node,
                        },
                    )
                })
                .collect(),
            rejected: selection
                .rejected
                .into_iter()
                .map(|r| Rejection {
                    id: r.id,
                    valid: r.valid,
                    reason: r.reason,
                })
                .collect(),
        }
    }

    pub fn runs_at_ingestion(candidate: &PhysicalCandidate, node: NodeId) -> bool {
        candidate
            .dag
            .nodes
            .iter()
            .find(|n| n.id == node)
            .expect("node")
            .output_state
            .timing
            == asap_types::ir::properties::ExecutionTiming::IngestionTime
    }
}

use stages::*;

// ── Example 1 workload ───────────────────────────────────────────────────

const Q1: &str = "sum by (job) (rate(http_requests_total[1m]))";
/// Count-Min + heap candidates: 32 logical, 8 of them also with Q2's sum
/// panes maintained.
const COUNT_MIN_WITHIN_LATENCY: usize = 40;
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
            // `http_requests_total` is a counter: its samples are never negative.
            metric_types: [("http_requests_total".into(), MetricType::Counter)].into(),
        }),
    }
}

// ── DAG helpers ──────────────────────────────────────────────────────────

/// Q2's local option, read off its summary build node. A whole-expression
/// sketch reads the raw samples of the range, absorbing `sum_over_time`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Q2Option {
    Exact,
    CountMinHeapPerJob,
    CountSketchHeapPerJob,
    WholeCountMinHeapPerJob,
    WholeCountSketchHeapPerJob,
    Hydra,
}

/// The logical (flat) and physical DAGs share node ids and payloads; the
/// helpers below read only those and each node's producers.
trait ExportedDag {
    fn producers(&self, consumer: NodeId) -> Vec<NodeId>;
    fn node_payload(&self, id: NodeId) -> &Payload;
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
    fn node_payload(&self, id: NodeId) -> &Payload {
        &self.nodes[id].operator
    }
}

impl ExportedDag for PhysicalASAPDAG {
    fn producers(&self, consumer: NodeId) -> Vec<NodeId> {
        let edges = self.edges.iter().filter(|e| e.consumer == consumer);
        edges.map(|e| e.producer).collect()
    }
    fn node_payload(&self, id: NodeId) -> &Payload {
        &self
            .nodes
            .iter()
            .find(|n| n.id == id)
            .expect("node")
            .payload
    }
}

/// Every node `root` depends on, including itself.
fn closure(dag: &impl ExportedDag, root: NodeId) -> HashSet<NodeId> {
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

fn payload(dag: &impl ExportedDag, id: NodeId) -> &Payload {
    dag.node_payload(id)
}

fn is_summary(payload: &Payload) -> bool {
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

/// Sketch families built in `nodes`, as Example 1's Q2 options.
fn sketch_options(dag: &impl ExportedDag, nodes: &HashSet<NodeId>) -> BTreeSet<Q2Option> {
    nodes
        .iter()
        .filter_map(|&id| match payload(dag, id) {
            Operator::ASAP(ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(kind, grouping),
                ..
            }) => {
                let whole = dag
                    .producers(id)
                    .iter()
                    .any(|&p| relational(payload(dag, p)).as_deref() == Some("time_range"));
                let per_job = matches!(grouping, GroupingStrategy::PerSubpopulationInstance);
                Some(match (grouping, kind.algorithm(), whole) {
                    (
                        GroupingStrategy::SharedMultiSubpopulation {
                            kind: HydraKind::HydraCms,
                            ..
                        },
                        _,
                        _,
                    ) => Q2Option::Hydra,
                    (_, SketchAlgorithm::CmsWithHeap, false) if per_job => {
                        Q2Option::CountMinHeapPerJob
                    }
                    (_, SketchAlgorithm::CmsWithHeap, true) if per_job => {
                        Q2Option::WholeCountMinHeapPerJob
                    }
                    (_, SketchAlgorithm::CountSketchWithHeap, false) if per_job => {
                        Q2Option::CountSketchHeapPerJob
                    }
                    (_, SketchAlgorithm::CountSketchWithHeap, true) if per_job => {
                        Q2Option::WholeCountSketchHeapPerJob
                    }
                    other => panic!("summary family outside Example 1: {other:?}"),
                })
            }
            _ => None,
        })
        .collect()
}

/// Exact accumulator kinds built in `nodes` (e.g. `["Rate", "Sum"]`); one
/// built in panes merged by a `SummaryMerge` is `"Sum panes"`.
fn exact_accumulators(dag: &impl ExportedDag, nodes: &HashSet<NodeId>) -> Vec<String> {
    let merged = |id| {
        nodes.iter().any(|&m| {
            matches!(payload(dag, m), Operator::ASAP(ASAPOp::SummaryMerge { .. }))
                && dag.producers(m).contains(&id)
        })
    };
    let mut kinds: Vec<_> = nodes
        .iter()
        .filter_map(|&id| match payload(dag, id) {
            Operator::ASAP(ASAPOp::SummaryAgg {
                family: FieldDataType::ExactAggregate(kind, _),
                ..
            }) => Some(match merged(id) {
                true => format!("{kind:?} panes"),
                false => format!("{kind:?}"),
            }),
            _ => None,
        })
        .collect();
    kinds.sort();
    kinds.dedup();
    kinds
}

fn roots(query_roots: &[NodeId]) -> (NodeId, NodeId) {
    assert_eq!(query_roots.len(), 2, "every candidate covers Q1 and Q2");
    (query_roots[0], query_roots[1])
}

/// Q2's option and whether Q1 and Q2 share any node, for one candidate.
fn classify(dag: &impl ExportedDag, query_roots: &[NodeId]) -> (Q2Option, bool) {
    let (q1, q2) = roots(query_roots);
    let (c1, c2) = (closure(dag, q1), closure(dag, q2));
    let options = sketch_options(dag, &c2);
    assert!(options.len() <= 1, "Q2 uses one local option: {options:?}");
    let option = options.into_iter().next().unwrap_or(Q2Option::Exact);
    (option, !c1.is_disjoint(&c2))
}

/// The non-ASAP operator's kind in snake case (`"scan"`, `"sort"`, …).
fn relational(payload: &Payload) -> Option<String> {
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

/// Example 1 with `http_requests_total` declared `metric_type`, or undeclared.
fn example1_workload_with(metric_type: Option<MetricType>) -> PlanningWorkload {
    let mut workload = example1_workload();
    let data = workload.data_workload.as_mut().expect("data workload");
    data.metric_types = metric_type
        .map(|t| [("http_requests_total".to_string(), t)].into())
        .unwrap_or_default();
    workload
}

fn pipeline() -> (
    PlanningWorkload,
    Vec<LogicalCandidate>,
    Vec<PhysicalCandidate>,
) {
    pipeline_for(example1_workload())
}

fn pipeline_for(
    workload: PlanningWorkload,
) -> (
    PlanningWorkload,
    Vec<LogicalCandidate>,
    Vec<PhysicalCandidate>,
) {
    let logical = stage1_logical_asap(&workload, &stage0_logical(&workload));
    let physical = stage2_physical(&workload, &logical);
    (workload, logical, physical)
}

/// One candidate's local choices: Q1's exact accumulators, Q2's top-k option
/// and Q2's exact accumulators.
type Choices = (Vec<String>, Q2Option, Vec<String>);

fn choices(dag: &impl ExportedDag, query_roots: &[NodeId]) -> Choices {
    let (q1, q2) = roots(query_roots);
    let (c1, c2) = (closure(dag, q1), closure(dag, q2));
    let (option, _) = classify(dag, query_roots);
    (
        exact_accumulators(dag, &c1),
        option,
        exact_accumulators(dag, &c2),
    )
}

/// The 44 Stage 1 combinations: Q1's rate and sum each raw or an exact
/// accumulator (4) × (Q2's top-k exact, Count-Min + heap or CountSketch +
/// heap (3) × Q2's sum_over_time raw, an exact accumulator or exact
/// accumulators over 10-s tumbling panes (3), or a whole-expression
/// Count-Min + heap or CountSketch + heap that absorbs sum_over_time (2)).
fn expected_choices() -> BTreeSet<Choices> {
    let q1 = [vec![], vec!["Rate"], vec!["Sum"], vec!["Rate", "Sum"]];
    let q2 = [
        Q2Option::Exact,
        Q2Option::CountMinHeapPerJob,
        Q2Option::CountSketchHeapPerJob,
    ];
    let owned = |kinds: &[&str]| kinds.iter().map(|k| k.to_string()).collect::<Vec<_>>();
    let mut all = BTreeSet::new();
    for a in &q1 {
        for &option in &q2 {
            for b in [vec![], vec!["Sum"], vec!["Sum panes"]] {
                all.insert((owned(a), option, owned(&b)));
            }
        }
        for option in [
            Q2Option::WholeCountMinHeapPerJob,
            Q2Option::WholeCountSketchHeapPerJob,
        ] {
            all.insert((owned(a), option, vec![]));
        }
    }
    all
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
    let roots = asap_frontend_promql::lower_promql_query_workload(&example1_workload(), 0)
        .expect("Example 1 lowers");
    let chains: Vec<Vec<String>> = roots
        .iter()
        .map(|root| {
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
fn stage0_one_summary_free_workload_dag() {
    let stage0 = stage0_logical(&example1_workload());
    roots(&stage0.query_roots);
    assert!(stage0.dag.nodes.iter().all(|n| !is_summary(&n.operator)));
}

/// Stage 0 keeps Q1 and Q2 separate; sharing is a Stage 1 decision.
#[test]
fn stage0_queries_do_not_share_nodes() {
    let stage0 = stage0_logical(&example1_workload());
    let (q1, q2) = roots(&stage0.query_roots);
    assert!(closure(&stage0.dag, q1).is_disjoint(&closure(&stage0.dag, q2)));
}

// ── Stage 1 ──────────────────────────────────────────────────────────────

/// Stage 1 outputs the 44 combinations twice: L1–L44 with separate inputs,
/// then L45–L88 with the shared input (Pass 2).
#[test]
fn stage1_has_88_candidates_covering_every_combination_twice() {
    let (_, logical, _) = pipeline();
    assert_eq!(logical.len(), 88);
    for (half, shared) in [(&logical[..44], false), (&logical[44..], true)] {
        let found: BTreeSet<_> = half
            .iter()
            .map(|c| choices(&c.dag, &c.query_roots))
            .collect();
        assert_eq!(found, expected_choices(), "each combination exactly once");
        for c in half {
            assert_eq!(classify(&c.dag, &c.query_roots).1, shared, "{}", c.id);
        }
    }
}

/// Q1 is exact in every Stage 1 candidate: no summary is reachable from its root.
#[test]
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

/// Q2's summary families are exactly Count-Min + heap and CountSketch + heap
/// per job, and Hydra over all jobs.
#[test]
#[ignore = "missing feature: Pass 1 has no Hydra alternative"]
fn stage1_q2_summary_families_are_heap_sketches_and_hydra() {
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
        BTreeSet::from([
            Q2Option::CountMinHeapPerJob,
            Q2Option::CountSketchHeapPerJob,
            Q2Option::WholeCountMinHeapPerJob,
            Q2Option::WholeCountSketchHeapPerJob,
            Q2Option::Hydra
        ])
    );
}

/// Sharing adds a variant and keeps the independent one, for every Q2 option
/// Pass 1 offers (Hydra: see `stage1_q2_summary_families_are_heap_sketches_and_hydra`).
#[test]
fn stage1_keeps_independent_and_shared_variants() {
    let (_, logical, _) = pipeline();
    let found: Vec<_> = logical
        .iter()
        .map(|c| classify(&c.dag, &c.query_roots))
        .collect();
    for option in [
        Q2Option::Exact,
        Q2Option::CountMinHeapPerJob,
        Q2Option::CountSketchHeapPerJob,
        Q2Option::WholeCountMinHeapPerJob,
        Q2Option::WholeCountSketchHeapPerJob,
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
fn stage1_candidates_are_valid_and_uniquely_named() {
    let (_, logical, _) = pipeline();
    let ids: BTreeSet<_> = logical.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids.len(), logical.len());
    for c in &logical {
        for root in &c.roots {
            root.validate_structure()
                .unwrap_or_else(|e| panic!("{}: {e}", c.id));
        }
    }
}

// ── Stage 2 ──────────────────────────────────────────────────────────────

/// No candidate is discarded before Stage 3: each of the 88 logical
/// candidates keeps its all-query-time physical candidate, and the 24 whose
/// Q2 sum comes in 10-s panes also get one with the panes at ingestion time
/// (the panels repeat predictably over arriving data) and one with the panes
/// kept at query time (B3): 136.
#[test]
fn stage2_keeps_every_logical_candidate() {
    let (_, logical, physical) = pipeline();
    assert_eq!(physical.len(), 136);
    let materialized: BTreeMap<_, usize> =
        physical.iter().fold(BTreeMap::new(), |mut counts, p| {
            *counts.entry(p.stage2.materialization.as_str()).or_default() += 1;
            counts
        });
    assert_eq!(
        materialized,
        BTreeMap::from([
            ("", 88),
            ("ingestion time: exact Sum ×6 panes", 24),
            ("query time, kept: exact Sum ×6 panes", 24),
        ])
    );
    let sources: BTreeSet<_> = physical.iter().map(|p| p.from_logical.as_str()).collect();
    let logical_ids: BTreeSet<_> = logical.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(sources, logical_ids);
    let ids: BTreeSet<_> = physical.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids.len(), physical.len());
}

/// Stage 2 preserves each logical candidate's Q2 option and input sharing,
/// except that maintained panes cannot share the scan with Q1, which reads it
/// at query time: the scan is copied per phase.
#[test]
fn stage2_preserves_logical_choices() {
    let (_, logical, physical) = pipeline();
    for p in &physical {
        let source = logical.iter().find(|c| c.id == p.from_logical).unwrap();
        let (option, shared) = classify(&source.dag, &source.query_roots);
        let maintained = p.stage2.materialization.starts_with("ingestion time");
        assert_eq!(
            classify(&p.dag, &p.query_roots),
            (option, shared && !maintained),
            "{} vs {}",
            p.id,
            source.id
        );
    }
}

/// Exact TopK is implemented as a sort followed by a limit.
#[test]
fn stage2_exact_topk_is_sort_then_limit() {
    let (_, _, physical) = pipeline();
    let exact: Vec<_> = physical
        .iter()
        .filter(|p| classify(&p.dag, &p.query_roots).0 == Q2Option::Exact)
        .collect();
    // 24 logical, 8 of which also maintain Q2's sum panes and 8 keep them.
    assert_eq!(exact.len(), 40);
    for p in exact {
        let sort_then_limit = p.dag.edges.iter().any(|e| {
            relational(payload(&p.dag, e.producer)).as_deref() == Some("sort")
                && relational(payload(&p.dag, e.consumer)).as_deref() == Some("limit")
        });
        assert!(sort_then_limit, "{}: no sort → limit", p.id);
    }
}

/// A summary Q2 is a build node feeding a top-10 estimation node; only
/// exact sums are merged from panes (top-k heaps do not merge).
#[test]
fn stage2_summary_topk_is_build_then_estimate() {
    let (_, _, physical) = pipeline();
    for p in &physical {
        for merge in p
            .dag
            .nodes
            .iter()
            .filter(|n| matches!(n.payload, Operator::ASAP(ASAPOp::SummaryMerge { .. })))
        {
            for pane in p.dag.producers(merge.id) {
                assert!(
                    matches!(
                        payload(&p.dag, pane),
                        Operator::ASAP(ASAPOp::SummaryAgg {
                            family: FieldDataType::ExactAggregate(..),
                            ..
                        })
                    ),
                    "{}: merges only exact panes",
                    p.id
                );
            }
        }
        if classify(&p.dag, &p.query_roots).0 == Q2Option::Exact {
            continue;
        }
        let build_to_estimate = p.dag.edges.iter().any(|e| {
            matches!(
                payload(&p.dag, e.producer),
                Operator::ASAP(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(..),
                    ..
                })
            ) && matches!(
                payload(&p.dag, e.consumer),
                Operator::ASAP(ASAPOp::SummaryEstimate { .. })
            )
        });
        assert!(build_to_estimate, "{}: no build → estimate", p.id);
    }
}

/// Only Q2's sum panes and their inputs run at ingestion time, and only in
/// the candidates that maintain them; every other node runs at query time.
#[test]
fn stage2_only_maintained_panes_run_at_ingestion_time() {
    let (_, _, physical) = pipeline();
    for p in &physical {
        let maintained = p.stage2.materialization.starts_with("ingestion time");
        let mut panes = 0;
        for n in &p.dag.nodes {
            if !runs_at_ingestion(p, n.id) {
                continue;
            }
            assert!(maintained, "{}: {:?} at ingestion", p.id, n.id);
            match &n.payload {
                Operator::ASAP(ASAPOp::SummaryAgg { .. }) => panes += 1,
                other => assert!(
                    matches!(
                        relational(other).as_deref(),
                        Some("scan" | "time_shift" | "time_range")
                    ),
                    "{}: {other:?} at ingestion",
                    p.id
                ),
            }
        }
        assert_eq!(panes, if maintained { 6 } else { 0 }, "{}", p.id);
    }
}

/// Compile `p` in the physical planner (the runtime capability check). Inputs
/// are what a deployment supplies: raw series for each sub-DAG the planner
/// runs as a retained PromQL expression, and the samples of each time range a
/// native operator reads.
fn compile_in_runtime(p: &PhysicalCandidate) -> Result<(), String> {
    use asap_executor::physical_planner::{compile, promql_fallback, InputContract};
    use asap_types::ir::physical_export::compile_physical_asap_workload_with_node_ids;
    use std::sync::Arc;
    let ids = compile_physical_asap_workload_with_node_ids(&p.stage2.roots)
        .expect("re-export")
        .node_ids;
    let mut inputs = BTreeMap::new();
    let mut pending = p.dag.roots.clone();
    let mut seen = HashSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        let node = ids.operator_node(id).expect("node");
        let time_range = relational(payload(&p.dag, id)).as_deref() == Some("time_range");
        // Raw samples a summary reads are an input, not a retained expression.
        let summary_input = time_range
            && p.dag.edges.iter().any(|e| {
                e.producer == id
                    && matches!(
                        payload(&p.dag, e.consumer),
                        Operator::ASAP(ASAPOp::SummaryAgg { .. })
                    )
            });
        let fallback = (!node.contains_asap() && !summary_input)
            .then(|| promql_fallback::raw_series(node).ok())
            .flatten();
        if let Some(selectors) = fallback {
            for (i, (_, schema)) in selectors.into_iter().enumerate() {
                let slot = promql_fallback::raw_series_input(id as u64, i);
                inputs.insert(slot, InputContract::bounded(schema));
            }
        } else if time_range {
            let schema = p.dag.nodes.iter().find(|n| n.id == id).unwrap();
            let schema = Arc::new(schema.output_schema.clone());
            inputs.insert(id as u64, InputContract::bounded(schema));
        } else {
            pending.extend(p.dag.producers(id));
        }
    }
    let roots: Vec<u64> = p.dag.roots.iter().map(|&id| id as u64).collect();
    compile(&p.dag, inputs, &roots)
        .map(|_| ())
        .map_err(|e| format!("{} ({}): {e}", p.id, p.label))
}

/// The physical planner compiles every candidate Stage 3 finds valid, and
/// rejects the invalid ones (Count-Min over weights not proven non-negative)
/// for the same reason Stage 3 gives. A candidate over Q2's 100 ms latency
/// bound compiles; the bound is Stage 3's alone. One keeping panes at query
/// time is rejected first for the executor's capability (Q56) and not
/// compared. Returns the number of candidates invalid for their weights.
fn assert_runtime_agrees_with_stage3(workload: PlanningWorkload) -> usize {
    let (workload, _, physical) = pipeline_for(workload);
    let selection = stage3_select(&workload, &physical, executor_models());
    let invalid: BTreeMap<_, _> = selection
        .rejected
        .iter()
        .filter(|r| {
            !r.valid
                && !r.reason.contains("latency bound")
                && !r.reason.contains("query time, kept")
        })
        .map(|r| (r.id.as_str(), r.reason.as_str()))
        .collect();
    for p in physical
        .iter()
        .filter(|p| p.dag.nodes.iter().all(|n| !n.kept))
    {
        let compiled = compile_in_runtime(p);
        match invalid.get(p.id.as_str()) {
            None => compiled.unwrap(),
            Some(reason) => {
                assert!(
                    reason.contains("CmsWithHeap needs non-negative update weights"),
                    "{}: {reason}",
                    p.id
                );
                let error = compiled.expect_err(&p.id);
                assert!(
                    error.contains("CMS requires a nonnegative weight contract"),
                    "{error}"
                );
            }
        }
    }
    invalid.len()
}

/// Runtime capability check (added by the implementer, not part of the
/// spec): with `http_requests_total` declared a counter, every candidate,
/// Count-Min + heap included, is valid in Stage 3 and compiles.
#[test]
fn stage2_runtime_compiles_every_candidate_over_a_declared_counter() {
    assert_eq!(assert_runtime_agrees_with_stage3(example1_workload()), 0);
}

/// Without the counter declaration, or with a gauge, the Count-Min + heap
/// candidates are invalid in Stage 3 and the runtime rejects them (those
/// within Q2's latency bound; the rest are rejected for latency first).
#[test]
fn stage2_count_min_needs_a_counter_declaration() {
    for metric_type in [None, Some(MetricType::Gauge)] {
        assert_eq!(
            assert_runtime_agrees_with_stage3(example1_workload_with(metric_type)),
            COUNT_MIN_WITHIN_LATENCY,
            "{metric_type:?}: the Count-Min + heap candidates"
        );
    }
}

/// A CountSketch+heap top-k readout compiles: the IR's derived readout schema
/// is the ranked-rows shape the runtime's keyed evaluation produces.
#[test]
fn stage2_count_sketch_heap_topk_compiles_in_the_physical_planner() {
    let (_, _, physical) = pipeline();
    let count_sketch: Vec<_> = physical
        .iter()
        .filter(|p| {
            p.dag.nodes.iter().any(|n| {
                matches!(&n.payload, Operator::ASAP(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(kind, _), ..
                }) if *kind.algorithm() == SketchAlgorithm::CountSketchWithHeap)
            })
        })
        .collect();
    assert!(!count_sketch.is_empty());
    for p in count_sketch {
        compile_in_runtime(p).unwrap();
    }
}

/// The plan Stage 3 selects compiles in the physical planner (added by the
/// implementer, not part of the spec).
#[test]
fn stage3_selected_plan_compiles_in_the_physical_planner() {
    let (workload, _, physical) = pipeline();
    let selection = stage3_select(&workload, &physical, executor_models());
    let selected = physical
        .iter()
        .find(|p| p.id == selection.selected)
        .unwrap();
    compile_in_runtime(selected).unwrap();
}

/// Deployment inputs (C2, added by the implementer): the reference
/// executor's exported capabilities reject no candidate its physical planner
/// compiles but those keeping panes at query time (B3), which it compiles as
/// recomputed at each evaluation but cannot keep (Q56). Planning with them
/// selects what the unrestricted default does, at the same costs for every
/// other candidate (both keep raw data, so no raw retention is priced).
#[test]
fn executor_capabilities_accept_every_compiled_candidate_and_keep_the_selection() {
    let (workload, _, physical) = pipeline();
    let executor = stage3_select(&workload, &physical, executor_models());
    let default = stage3_select(&workload, &physical, PlanningModels::builtin());
    let mut compiled = 0;
    for p in &physical {
        if compile_in_runtime(p).is_ok() {
            compiled += 1;
            if let Some(r) = executor.rejected.iter().find(|r| r.id == p.id) {
                let kept = p.dag.nodes.iter().any(|n| n.kept);
                assert_eq!(
                    r.reason.contains("deployment"),
                    kept,
                    "{}: {}",
                    p.id,
                    r.reason
                );
            }
        }
    }
    assert_eq!(compiled, physical.len());
    assert_eq!(executor.selected, default.selected);
    let totals = |s: &Selection| -> BTreeMap<String, f64> {
        s.costs
            .iter()
            .map(|(id, c)| (id.clone(), c.total))
            .collect()
    };
    let mut default_totals = totals(&default);
    default_totals.retain(|id, _| executor.costs.contains_key(id));
    assert_eq!(totals(&executor), default_totals);
}

// ── Stage 3 ──────────────────────────────────────────────────────────────

/// Stage 3 selects one candidate and gives every other one a reason.
#[test]
fn stage3_selects_one_and_explains_the_rest() {
    let (workload, _, physical) = pipeline();
    let selection = stage3_select(&workload, &physical, executor_models());
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
fn stage3_selects_cheapest_valid() {
    let (workload, _, physical) = pipeline();
    let selection = stage3_select(&workload, &physical, executor_models());
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

/// Q2 asks for 100 ms (S6): Stage 3 rejects exactly the candidates whose
/// query-time work for Q2 takes longer in one evaluation, at one cost unit
/// (a CPU-ms) per ms. Count-Sketch + heap is over the bound in every form;
/// the selected plan is within it.
#[test]
fn stage3_rejects_candidates_over_the_latency_bound() {
    let (workload, _, physical) = pipeline();
    let selection = stage3_select(&workload, &physical, executor_models());
    let over: BTreeSet<_> = selection
        .rejected
        .iter()
        .filter(|r| !r.valid && r.reason.contains("latency bound"))
        .map(|r| {
            assert!(r.reason.starts_with("q2: "), "{}: {}", r.id, r.reason);
            r.id.as_str()
        })
        .collect();
    assert!(!over.is_empty());
    assert!(!over.contains(selection.selected.as_str()));
    for p in &physical {
        let count_sketch = p.dag.nodes.iter().any(|n| {
            matches!(&n.payload, Operator::ASAP(ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(kind, _), ..
            }) if *kind.algorithm() == SketchAlgorithm::CountSketchWithHeap)
        });
        // A kept candidate is rejected first for the executor's capability.
        if count_sketch && !p.dag.nodes.iter().any(|n| n.kept) {
            assert!(over.contains(p.id.as_str()), "{}", p.id);
        }
    }
}

/// Per-second cost keeps Example 1's ranking: both panels repeat every
/// 10 s, so every all-query-time candidate costs 0.1 × its per-evaluation
/// cost, and P82 (P60 before Q2's tumbling sums renumbered the candidates)
/// still wins at 46.201 × 0.1 per second. A candidate that maintains panes
/// pays ingestion-time work and memory that do not depend on the cadence.
#[test]
fn stage3_per_second_cost_keeps_the_ranking() {
    let (workload, _, physical) = pipeline();
    let per_second = stage3_select(&workload, &physical, executor_models());
    // Evaluated once per second, a candidate's cost is its per-evaluation cost.
    let mut every_second = workload.clone();
    for entry in every_second
        .query_workload
        .repeating_queries
        .iter_mut()
        .flatten()
    {
        entry.demand = RepeatedDemand::FixedInterval(RepetitionInterval(1_000));
    }
    let per_evaluation = stage3_select(&every_second, &physical, executor_models());
    assert_eq!(per_second.selected, "P82");
    assert_eq!(per_evaluation.selected, "P82");
    for (id, cost) in per_second.costs.iter().filter(|(id, _)| !id.contains("-m")) {
        let expected = 0.1 * per_evaluation.costs[id].total;
        assert!((cost.total - expected).abs() <= 1e-9 * expected, "{id}");
    }
    let best = per_second.costs["P82"].total;
    assert!((best - 4.6201).abs() < 1e-3, "{best}");
}

/// Every node is charged exactly once, so a shared input is costed once for
/// both queries. Stage 3 prices valid candidates only (user decision).
#[test]
fn stage3_charges_each_node_once() {
    let (workload, _, physical) = pipeline();
    let selection = stage3_select(&workload, &physical, executor_models());
    let invalid: BTreeSet<_> = selection
        .rejected
        .iter()
        .filter(|r| !r.valid)
        .map(|r| r.id.as_str())
        .collect();
    for p in &physical {
        if invalid.contains(p.id.as_str()) {
            assert!(!selection.costs.contains_key(&p.id), "{} is priced", p.id);
            continue;
        }
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

/// Sharing the input never costs more than reading it separately, for the
/// same local choices (Hydra: see
/// `stage1_q2_summary_families_are_heap_sketches_and_hydra`).
#[test]
fn stage3_shared_input_is_not_costlier() {
    let (workload, _, physical) = pipeline();
    let selection = stage3_select(&workload, &physical, executor_models());
    // All query time: a maintained-pane candidate cannot share the scan.
    let by_combo: BTreeMap<_, _> = physical
        .iter()
        .filter(|p| p.stage2.materialization.is_empty())
        .filter_map(|p| {
            let cost = selection.costs.get(&p.id)?.total;
            let shared = classify(&p.dag, &p.query_roots).1;
            Some(((choices(&p.dag, &p.query_roots), shared), cost))
        })
        .collect();
    // Count-Sketch + heap misses Q2's 100 ms latency bound in every form.
    for option in [
        Q2Option::Exact,
        Q2Option::CountMinHeapPerJob,
        Q2Option::WholeCountMinHeapPerJob,
    ] {
        assert!(
            by_combo.keys().any(|((_, o, _), _)| *o == option),
            "{option:?} has no priced candidate"
        );
    }
    for ((choice, shared), cost) in &by_combo {
        if *shared {
            let separate = by_combo[&(choice.clone(), false)];
            assert!(*cost <= separate, "{choice:?}");
        }
    }
    assert!(
        by_combo.keys().any(|(_, shared)| *shared),
        "no shared variant"
    );
}

/// The selected plan shares the input, the doc's "Raw with a shared input"
/// winner; its saving over the same choices read separately is exactly one
/// scan and one range node, priced once instead of twice.
#[test]
fn stage3_selects_a_shared_input_plan() {
    let (workload, _, physical) = pipeline();
    let selection = stage3_select(&workload, &physical, executor_models());
    let selected = physical
        .iter()
        .find(|p| p.id == selection.selected)
        .unwrap();
    assert!(classify(&selected.dag, &selected.query_roots).1);
    let separate = physical
        .iter()
        .find(|p| {
            !classify(&p.dag, &p.query_roots).1
                && choices(&p.dag, &p.query_roots) == choices(&selected.dag, &selected.query_roots)
        })
        .unwrap();
    let input_cost: f64 = selected
        .dag
        .nodes
        .iter()
        .filter(|n| {
            matches!(
                relational(&n.payload).as_deref(),
                Some("scan" | "time_range")
            )
        })
        .map(|n| selection.costs[&selected.id].per_node[&n.id])
        .sum();
    let saving = selection.costs[&separate.id].total - selection.costs[&selected.id].total;
    assert!(
        (saving - input_cost).abs() < 1e-9,
        "{saving} vs {input_cost}"
    );
}

/// Every Q2 realization, exact or sketch, whole-expression or not, returns
/// the same selected-rows schema (#579) in Stage 1, so consumers see one
/// shape. (Stage 2 exports exact top-k as sort → limit over the per-series
/// rows, whose schema still carries `ts`.)
#[test]
fn q2_roots_keep_one_schema_across_realizations() {
    let (_, logical, _) = pipeline();
    let schemas: Vec<_> = logical
        .iter()
        .map(|c| {
            (
                classify(&c.dag, &c.query_roots).0,
                c.roots[1].schema.clone(),
            )
        })
        .collect();
    assert!(schemas
        .iter()
        .any(|(o, _)| *o == Q2Option::WholeCountSketchHeapPerJob));
    for (option, schema) in &schemas {
        assert_eq!(schema, &schemas[0].1, "{option:?}");
    }
}
