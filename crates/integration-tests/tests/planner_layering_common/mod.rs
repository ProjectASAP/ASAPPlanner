//! Shared helpers for the #509 Examples 1–4 acceptance tests
//! (`planner_layering_example{1,2,3,4}.rs`). Specs:
//! `docs/design_docs/proposals/planner-layering-example{1,2,3,4}-acceptance.md`.
//!
//! [`run_stages`] runs the real stages through
//! `asap_devtools::planning_stages::run_planning_stages`, the pipeline
//! `stage_pipeline` also runs; the example workloads come from
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
use asap_logical_asap_optimization::DeploymentCapabilities;
use asap_plan_selection::reference_pricer::WholePlanSelection as Selection;
use asap_types::ir::flat::{flatten, FlatDag, NodeId};
use asap_types::ir::physical_export::PhysicalASAPDAG;
use asap_types::ir::schema::{FieldDataType, SketchAlgorithm, SketchParams, SketchStatistic};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, QueryRoot};
use asap_types::physical::Materialization;
use asap_types::physical::PhysicalWorkloadPlan;
use asap_types::workload::{PlanningWorkload, QueryRecurrence, RepeatedDemand};

pub use asap_devtools::planner_layering_examples::*;

pub type Payload = Operator<NodeId>;

/// Lower and plan a PromQL workload.
pub fn run_promql(workload: &PlanningWorkload) -> Run {
    run_stages(workload, lower_promql(workload))
}

/// Lower and plan a PromQL workload, selecting under `settings` (a cost
/// model other than the default).
pub fn run_promql_under(workload: &PlanningWorkload, settings: &CostInputs) -> Run {
    run_stages_under(workload, lower_promql(workload), settings)
}

/// Lower and plan a PromQL workload for a deployment with `deployment`'s
/// capabilities, under the default cost model.
pub fn run_promql_for(workload: &PlanningWorkload, deployment: &DeploymentCapabilities) -> Run {
    run_stages_with(
        workload,
        lower_promql(workload),
        &CostInputs::default(),
        deployment,
    )
}

/// Whether `form` answers a query window by merging several summaries.
pub fn needs_merge(form: WindowForm, query_window_ms: u64) -> bool {
    match form {
        WindowForm::None => false,
        WindowForm::Sliding { length_ms, .. } => length_ms < query_window_ms,
        WindowForm::Tumbling { .. } | WindowForm::ExponentialHistogram { .. } => true,
    }
}

/// `roots` with every identical sub-DAG made one node (the identical-
/// expression rule, as the memo's group identity applies it).
#[allow(dead_code)]
pub fn share_identical(roots: &[QueryRoot]) -> Vec<QueryRoot> {
    asap_logical_asap_optimization::pass2::cse::share_common_sub_dags(
        roots
            .iter()
            .enumerate()
            .map(|(id, root)| match root {
                QueryRoot::Operator(node) => (id, std::rc::Rc::clone(node)),
                QueryRoot::Scalar(_) => panic!("operator root"),
            })
            .collect(),
    )
    .into_iter()
    .map(|(_, node)| QueryRoot::Operator(node))
    .collect()
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

/// The number of distinct nodes (by identity) `roots` reach: smaller after
/// [`share_identical`] exactly when some sub-DAG was shared.
#[allow(dead_code)]
pub fn distinct_nodes(roots: &[QueryRoot]) -> usize {
    roots
        .iter()
        .filter_map(|root| match root {
            QueryRoot::Operator(node) => Some(node),
            QueryRoot::Scalar(_) => None,
        })
        .flat_map(asap_types::ir::OperatorNode::reachable)
        .map(|node| std::rc::Rc::as_ptr(&node))
        .collect::<std::collections::HashSet<_>>()
        .len()
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
            .filter(|r| r.cause.failed_a_check())
            .map(|r| (r.id.as_str(), r.reason.as_str()))
            .collect()
    }

    pub fn cost(&self, id: &str) -> Option<f64> {
        self.selection.costs.get(id).and_then(|c| c.cost_per_second)
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

/// The bounded production stages over `roots`, retaining the selected plan
/// and search diagnostics. Tests that require a particular algorithm or window
/// use `run_explicit_candidates` instead of assuming it appears in the archive.
pub fn run_stages(workload: &PlanningWorkload, roots: Vec<QueryRoot>) -> Run {
    run_stages_under(workload, roots, &CostInputs::default())
}

/// [`run_stages`], selecting under `settings`.
pub fn run_stages_under(
    workload: &PlanningWorkload,
    roots: Vec<QueryRoot>,
    settings: &CostInputs,
) -> Run {
    run_stages_with(
        workload,
        roots,
        settings,
        &DeploymentCapabilities::default(),
    )
}

/// [`run_stages`], selecting under `settings` for a deployment with
/// `deployment`'s capabilities.
pub fn run_stages_with(
    workload: &PlanningWorkload,
    roots: Vec<QueryRoot>,
    settings: &CostInputs,
    deployment: &DeploymentCapabilities,
) -> Run {
    let models = asap_planner::PlanningModels::builtin()
        .with_plan_selection(settings)
        .with_deployment_capabilities(deployment);
    run_stages_over(workload, roots, models)
}

fn run_stages_over(
    workload: &PlanningWorkload,
    roots: Vec<QueryRoot>,
    models: asap_planner::PlanningModels<'_>,
) -> Run {
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap_or_default();
    // The facade's run, with the reference pricer over the selected round's
    // memo choices: the whole-plan costs and rejections these tests read.
    let run = run_planning_stages_over(roots, &entries, &data, None, models)
        .expect("planning stages run");
    let reference = run.reference.expect("the memo is enumerated");
    let logical_candidates = reference
        .choices
        .into_iter()
        .map(|c| {
            let roots = c.roots.unwrap_or_else(|e| panic!("{} composes: {e}", c.id));
            logical(c.id, roots)
        })
        .collect();
    let physical = reference
        .physical
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
            run.stage0_query_language_frontends,
        ),
        logical: logical_candidates,
        physical,
        pruned: reference.stage2_failed,
        selection: reference.selection.expect("plan selection selects"),
    }
}

/// Evaluate deliberately supplied logical configurations. This fixture helper
/// does not search or enumerate cross-query combinations; it exercises the
/// existing physical rules and evaluator on the computations named by a test.
/// Production-search tests continue to use `run_stages`.
pub fn run_explicit_candidates(
    workload: &PlanningWorkload,
    original_roots: Vec<QueryRoot>,
    candidates: Vec<Vec<QueryRoot>>,
    models: asap_planner::PlanningModels<'_>,
) -> Run {
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap_or_default();
    let targets: Vec<_> = entries
        .iter()
        .map(|entry| entry.requirements.accuracy.clone())
        .collect();
    // Explicit candidates are composed outside the memo: their summaries are
    // sized as Pass 1 sizes a candidate, which Stage 3 only checks.
    let logical: Vec<_> = candidates
        .into_iter()
        .enumerate()
        .map(|(index, roots)| {
            let roots = asap_accuracy::propagation::recompose_guarantees(&roots)
                .expect("explicit fixture contracts compose");
            logical(
                format!("P{}", index + 1),
                asap_accuracy::sizing::with_summaries_sized_for_targets(&roots, &targets, &data)
                    .expect("explicit fixture summaries rebuild"),
            )
        })
        .collect();
    let mut physical = Vec::new();
    for candidate in &logical {
        for (index, mut plan) in asap_physical_asap_optimization::optimize_physical_asap(
            &candidate.id,
            &candidate.roots,
            &entries,
            &data,
            models.deployment_capabilities.query_time_results_retained,
        )
        .expect("explicit computation has executable physical configurations")
        .into_iter()
        .enumerate()
        {
            plan.id = if index == 0 {
                candidate.id.clone()
            } else {
                format!("{}-m{}", candidate.id, index)
            };
            physical.push(Physical {
                id: plan.id.clone(),
                from_logical: plan.from_logical.clone(),
                query_roots: plan.dag.roots.clone(),
                candidate: plan,
            });
        }
    }
    // Stage 2's deployment rules (raw data retention) prune the plans the
    // deployment cannot run before Stage 3 prices the rest.
    let mut plans: Vec<_> = physical.iter().map(|plan| plan.candidate.clone()).collect();
    asap_devtools::memo_pricing::retain_deployable(&mut plans, models);
    physical.retain(|p| plans.iter().any(|plan| plan.id == p.id));
    let selection = asap_devtools::memo_pricing::reference_select(&plans, &entries, &data, models)
        .expect("explicit fixture contains a valid physical configuration");
    Run {
        stage0_query_language_frontends: logical_stage0(original_roots),
        logical,
        physical,
        pruned: Vec::new(),
        selection,
    }
}

pub fn run_pattern_b_fixture(
    workload: &PlanningWorkload,
    settings: &CostInputs,
    deployment: &DeploymentCapabilities,
) -> Run {
    run_explicit_candidates(
        workload,
        lower_promql(workload),
        pattern_b_computations(workload),
        asap_planner::PlanningModels::builtin()
            .with_plan_selection(settings)
            .with_deployment_capabilities(deployment),
    )
}

/// Three fixed Pattern-B computations used by cost/retention fixtures:
/// exact, whole-window KLL, and a KLL chain of sixty-second panes.
pub fn pattern_b_computations(workload: &PlanningWorkload) -> Vec<Vec<QueryRoot>> {
    use asap_logical_asap_optimization::pass1::per_computation_candidates::{
        compose_logical_candidate, generate_per_computation_candidates,
        HowToComputeAggregateMeasure,
    };
    use asap_logical_asap_optimization::pass2::asap_aware_cse::{
        pane_candidates, window_composition_pass2_candidates,
    };
    let roots = lower_promql(workload);
    let local = generate_per_computation_candidates(
        &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(&roots),
    )
    .unwrap();
    assert_eq!(
        local.targets.len(),
        1,
        "Pattern B has one quantile computation"
    );
    let kll = local.targets[0]
        .ways_to_compute
        .iter()
        .position(|candidate| {
            matches!(
                candidate,
                HowToComputeAggregateMeasure::Summary(SketchAlgorithm::Kll)
            )
        })
        .unwrap();
    let whole: Vec<_> = compose_logical_candidate(&local, &[kll]).unwrap();
    let panes = pane_candidates(&whole, &[Some(60_000)]);
    assert_eq!(panes.len(), 1);
    let chain = window_composition_pass2_candidates(&whole, &panes)
        .unwrap()
        .remove(0)
        .roots
        .expect("sixty-second panes compose");
    vec![roots, whole, chain]
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
            } | ASAPOp::PaneChain {
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

/// The sketch build nodes (`SummaryAgg` or tumbling-pane `PaneChain` over a
/// sketch family) of `dag`, with their algorithm and parameters.
pub fn sketch_builds(dag: &impl ExportedDag) -> Vec<(NodeId, SketchAlgorithm, SketchParams)> {
    dag.node_ids()
        .into_iter()
        .filter_map(|id| match dag.payload(id) {
            Operator::ASAP(
                ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(kind, _),
                    ..
                }
                | ASAPOp::PaneChain {
                    family: FieldDataType::Sketch(kind, _),
                    ..
                },
            ) => Some((id, *kind.algorithm(), kind.params().clone())),
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

/// No candidate is discarded before plan selection: physical ASAP-aware
/// optimization gives each logical candidate `L<n>` exactly one physical
/// candidate with nothing materialized, `P<n>`, and one `P<n>-m<k>` for each
/// other materialization assignment.
pub fn assert_one_unmaterialized_physical_candidate_per_logical(run: &Run) {
    assert!(run.pruned.is_empty(), "pruned: {:?}", run.pruned);
    let unmaterialized: Vec<_> = run
        .physical
        .iter()
        .filter(|p| !p.id.contains("-m"))
        .collect();
    let sources: BTreeSet<_> = unmaterialized
        .iter()
        .map(|p| p.from_logical.as_str())
        .collect();
    let logical: BTreeSet<_> = run.logical.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(sources, logical);
    assert_eq!(unmaterialized.len(), run.logical.len());
    for p in &run.physical {
        let base = p.id.split("-m").next().unwrap();
        assert_eq!(&base[1..], &p.from_logical[1..], "{}", p.id);
        let unmaterialized = !p.id.contains("-m");
        assert_eq!(unmaterialized, !p.candidate.materializes_any(), "{}", p.id);
        if unmaterialized {
            for n in &p.dag().nodes {
                assert_eq!(
                    materialization(p, n.id),
                    MaterializationOption::NotMaterialized
                );
            }
        }
    }
    let ids: BTreeSet<_> = run.physical.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids.len(), run.physical.len(), "unique physical ids");
}

/// The physical candidates with nothing materialized, one per logical
/// candidate.
pub fn unmaterialized(run: &Run) -> impl Iterator<Item = &Physical> {
    run.physical
        .iter()
        .filter(|p| !p.candidate.materializes_any())
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
        // Totals within rounding of each other tie (plan selection's rule).
        let cost = run.cost(&p.id).unwrap();
        assert!(best <= cost + cost.abs() * 1e-9, "{} is cheaper", p.id);
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
        let sum: f64 = cost
            .per_node
            .values()
            .map(|c| c.cost_per_second.unwrap())
            .sum();
        assert!(
            (cost.cost_per_second.unwrap() - sum).abs() <= 1e-9 * sum.abs().max(1.0),
            "{}",
            p.id
        );
        let times = computations(p.dag());
        for (id, c) in &cost.per_node {
            assert_eq!(c.multiplier, times[id], "{}: multiplier of {id:?}", p.id);
        }
    }
}

/// How many times each node is computed per evaluation of its consumers (#509; the
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

// ── Materialization ──────────────────────────────────────────────────────

/// A node's materialization option (#509 "Materialization"), without its
/// storage medium.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MaterializationOption {
    /// Runs as data arrives; output stored before any query asks.
    MatAtIngestion,
    /// Runs when a query first needs it; output reused by the other
    /// queries of the evaluation, or, as `MatAtQueryKeptAcrossEvaluations`,
    /// kept for later evaluations.
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
        Materialization::MatAtQuery(_) | Materialization::MatAtQueryKeptAcrossEvaluations(_) => {
            MaterializationOption::MatAtQuery
        }
        Materialization::NotMaterialized => MaterializationOption::NotMaterialized,
    }
}

pub fn runs_at_ingestion(p: &Physical, node: NodeId) -> bool {
    let n = p.dag().nodes.iter().find(|n| n.id == node).expect("node");
    n.output_state.timing == asap_types::ir::properties::ExecutionTiming::IngestionTime
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
    /// Rebuilt from the query window's raw data samples (no window summary).
    None,
    /// Windows of `length_ms` starting every `slide_ms`.
    Sliding { length_ms: u64, slide_ms: u64 },
    /// Back-to-back windows of `length_ms`.
    Tumbling { length_ms: u64 },
    /// EH buckets covering `horizon_ms` of history.
    ExponentialHistogram { horizon_ms: u64 },
}

/// The window summary of a build: a `PaneChain` keeps back-to-back panes of
/// its pane width (tumbling); a `SummaryAgg` is rebuilt from its query
/// window. Sliding windows and Exponential Histograms are not planned yet
/// (#628).
pub fn window_form(dag: &impl ExportedDag, build: NodeId) -> WindowForm {
    match dag.payload(build) {
        Operator::ASAP(ASAPOp::PaneChain { pane_width_ms, .. }) => WindowForm::Tumbling {
            length_ms: *pane_width_ms,
        },
        _ => WindowForm::None,
    }
}

/// How far back from an evaluation a materialized node's kept output
/// reaches in event time, derived the way plan selection prices what it
/// keeps: a pane chain keeps its `N`
/// panes of width `w`, plus the one being built when maintained at
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
        Operator::ASAP(ASAPOp::PaneChain {
            pane_width_ms,
            pane_count,
            ..
        }) => {
            let panes = u64::from(*pane_count) + u64::from(at_ingestion);
            Some(panes * pane_width_ms + offset_below(below(node)))
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
