//! The contracts: invariants every stage's output keeps, checked on every
//! workload of the corpus registry (`docs/develop_docs/testing.md`,
//! "Contracts"). A violation names the workload and the invariant.
//!
//! - every id resolves (memo groups, candidates, logical and physical
//!   edges, physical candidates, plan nodes), and every surviving
//!   candidate's input groups have a surviving candidate;
//! - every edge the plan uses is logically and physically admissible;
//! - the selection MILP's objective is `plan_cost` of the plan (relative
//!   1e-9);
//! - every node's cost is ≥ 0;
//! - every query root is served by a plan node of its query's group;
//! - constraint (K) holds on the plan: each query's error bound, composed
//!   over the plan from its readouts' requirements with the operators'
//!   error contracts, is within its target.
//!
//! Edge admissibility, MILP objective = `plan_cost` and (K) are consistency
//! checks: each reuses the planner code it checks (the testing doc says
//! which).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use asap_cost::CostInputs;
use asap_devtools::corpus_registry::{corpus_workloads, CorpusWorkload, PlannedWorkload, SOURCES};
use asap_devtools::planning_stages::RunUntil;
use asap_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPNodeId};
use asap_types::ir::properties::{AccuracyMetric, AccuracyTarget};
use asap_types::stages::{
    CandidateForMemoGroupId, LogicalRequirements, Memo, MemoGroupId, PhysicalASAPMemoCandidates,
    PhysicalCandidateId,
};

/// The relative tolerance of MILP objective = `plan_cost`.
const OBJECTIVE_RELATIVE_TOLERANCE: f64 = 1e-9;

/// Every contract violation of `planned` (planned with `cost_inputs`), each
/// a line naming the invariant; empty when every contract holds. A workload
/// Stage 3 selects no plan for violates "every query root is served".
pub fn contract_violations(planned: &PlannedWorkload, cost_inputs: &CostInputs) -> Vec<String> {
    checked_contracts(planned, cost_inputs).violations
}

/// What checking the contracts on one workload found.
struct CheckedContracts {
    violations: Vec<String>,
    /// The targeted queries whose composed error at the root is nonzero in
    /// some metric: (K) had something to check.
    queries_with_composed_error: usize,
}

fn checked_contracts(planned: &PlannedWorkload, cost_inputs: &CostInputs) -> CheckedContracts {
    let found = |violations| CheckedContracts {
        violations,
        queries_with_composed_error: 0,
    };
    let run = &planned.stages.run;
    let (Some(logical), Some(physical)) =
        (&run.logical_asap_candidates, &run.physical_asap_candidates)
    else {
        return found(vec![
            "every query root is served: Pass 2 or Stage 2 produced nothing".into(),
        ]);
    };
    let memo = &logical.memo;
    let mut violations = memo_ids_resolve(memo, logical);
    violations.extend(physical_ids_resolve(physical));
    if !violations.is_empty() {
        // The checks below look candidates up by these ids.
        return found(violations);
    }
    let (plan, selection) = match &run.selection {
        Ok(selected) => selected,
        Err(error) => {
            violations.extend(physical_inputs_have_producers(physical));
            violations.push(format!("every query root is served: no plan ({error})"));
            return found(violations);
        }
    };
    violations.extend(plan_ids_resolve(plan, physical));
    if !violations.is_empty() {
        // The checks below index by these ids.
        return found(violations);
    }
    violations.extend(physical_inputs_have_producers(physical));
    violations.extend(plan_edges_are_admissible(plan, physical));
    violations.extend(query_roots_are_served(plan, physical));
    match asap_cost::plan_cost(plan, &planned.entries, &planned.data, cost_inputs) {
        Err((node, error)) => violations.push(format!("plan_cost fails at node {node}: {error}")),
        Ok((cost, _)) => {
            for (node, node_cost) in &cost.per_node {
                if let Some(per_second) = node_cost.cost_per_second {
                    if per_second.is_nan() || per_second < 0.0 {
                        violations.push(format!(
                            "every node cost is >= 0: node {node} costs {per_second}/s ({})",
                            node_cost.detail
                        ));
                    }
                }
            }
            if cost.cost_per_second.is_none() {
                violations.push(
                    "MILP objective = plan_cost: plan_cost has no cost per second for a selected \
                     plan"
                        .into(),
                );
            }
            if let Some(plan_cost) = cost.cost_per_second {
                let objective = selection.milp_objective;
                if (objective - plan_cost).abs()
                    > OBJECTIVE_RELATIVE_TOLERANCE * plan_cost.abs().max(f64::MIN_POSITIVE)
                {
                    violations.push(format!(
                        "MILP objective = plan_cost: objective {objective:e}/s, plan_cost \
                         {plan_cost:e}/s (relative difference {:e})",
                        (objective - plan_cost).abs() / plan_cost.abs()
                    ));
                }
            }
        }
    }
    let (k_violations, queries_with_composed_error) = constraint_k_holds(plan, physical);
    violations.extend(k_violations);
    CheckedContracts {
        violations,
        queries_with_composed_error,
    }
}

/// The ids of the memo and of Pass 2's edges and pruned list resolve, and
/// every surviving candidate's input groups have a surviving candidate.
fn memo_ids_resolve(
    memo: &Memo,
    logical: &asap_types::stages::LogicalASAPMemoCandidates,
) -> Vec<String> {
    let mut violations = Vec::new();
    let groups = memo.groups();
    let group_exists = |group: &MemoGroupId| (group.0 as usize) < groups.len();
    let candidate_exists = |id: &CandidateForMemoGroupId| {
        group_exists(&id.group) && id.index_in_group < groups[id.group.0 as usize].candidates.len()
    };
    let pruned: BTreeSet<CandidateForMemoGroupId> =
        logical.pruned.iter().map(|p| p.candidate).collect();
    for (index, group) in groups.iter().enumerate() {
        for parent in &group.parents {
            if !group_exists(parent) {
                violations.push(format!("ids resolve: group G{index}'s parent {parent:?}"));
            }
        }
        for child in group.expression.children() {
            if !group_exists(child) {
                violations.push(format!("ids resolve: group G{index}'s input {child:?}"));
            }
        }
        for (position, candidate) in group.candidates.iter().enumerate() {
            let id = CandidateForMemoGroupId {
                group: MemoGroupId(index as u32),
                index_in_group: position,
            };
            let inputs = candidate.expression.operator.children();
            if candidate.input_requirements.len() != inputs.len() {
                violations.push(format!(
                    "ids resolve: {id:?} has {} input requirements for {} inputs",
                    candidate.input_requirements.len(),
                    inputs.len()
                ));
            }
            for input in inputs
                .iter()
                .chain(&candidate.fused_groups.iter().collect::<Vec<_>>())
            {
                if !group_exists(input) {
                    violations.push(format!("ids resolve: {id:?} reads {input:?}"));
                }
            }
            if pruned.contains(&id) {
                continue;
            }
            for fused in &candidate.fused_groups {
                let alive = groups.get(fused.0 as usize).is_some_and(|fused_group| {
                    (0..fused_group.candidates.len()).any(|position| {
                        !pruned.contains(&CandidateForMemoGroupId {
                            group: *fused,
                            index_in_group: position,
                        })
                    })
                });
                if !alive {
                    violations.push(format!(
                        "every candidate's inputs exist: {id:?} fuses {fused:?}, which has no \
                         surviving candidate"
                    ));
                }
            }
            for input in inputs {
                let Some(input_group) = groups.get(input.0 as usize) else {
                    continue;
                };
                let alive = (0..input_group.candidates.len()).any(|position| {
                    !pruned.contains(&CandidateForMemoGroupId {
                        group: *input,
                        index_in_group: position,
                    })
                });
                if !alive {
                    violations.push(format!(
                        "every candidate's inputs exist: {id:?} reads {input:?}, which has no \
                         surviving candidate"
                    ));
                }
            }
        }
    }
    for (q, query) in memo.queries.iter().enumerate() {
        for group in query.root.operator_refs() {
            if !group_exists(group) {
                violations.push(format!("ids resolve: query {q}'s root {group:?}"));
            }
        }
    }
    for edge in &logical.edges {
        if !candidate_exists(&edge.producer_candidate_id)
            || !candidate_exists(&edge.consumer_candidate_id)
        {
            violations.push(format!("ids resolve: logical edge {edge:?}"));
            continue;
        }
        if pruned.contains(&edge.producer_candidate_id)
            || pruned.contains(&edge.consumer_candidate_id)
        {
            violations.push(format!(
                "ids resolve: logical edge {edge:?} joins a candidate Pass 2 pruned"
            ));
        }
        let consumer = memo.candidate(edge.consumer_candidate_id);
        let input = consumer
            .expression
            .operator
            .children()
            .get(edge.input_index_of_consumer)
            .copied()
            .copied();
        if input != Some(edge.producer_candidate_id.group) {
            violations.push(format!(
                "ids resolve: logical edge {edge:?} reads {:?}, not the consumer's input {input:?}",
                edge.producer_candidate_id.group
            ));
        }
    }
    for pruned in &logical.pruned {
        if !candidate_exists(&pruned.candidate) {
            violations.push(format!("ids resolve: pruned {:?}", pruned.candidate));
        }
    }
    violations
}

/// Stage 2's candidates name logical candidates that exist and survived
/// Pass 2, and its edges join a producer of the consumer's input group.
fn physical_ids_resolve(physical: &PhysicalASAPMemoCandidates) -> Vec<String> {
    let mut violations = Vec::new();
    let logical = &physical.logical_candidates;
    let memo = &logical.memo;
    let pruned: BTreeSet<CandidateForMemoGroupId> =
        logical.pruned.iter().map(|p| p.candidate).collect();
    let resolves = |id: CandidateForMemoGroupId| {
        memo.groups()
            .get(id.group.0 as usize)
            .is_some_and(|group| id.index_in_group < group.candidates.len())
    };
    for (index, candidate) in physical.physical_candidates.iter().enumerate() {
        let id = candidate.logical_candidate_id;
        if !resolves(id) {
            violations.push(format!(
                "ids resolve: physical candidate {index} names {id:?}"
            ));
        } else if pruned.contains(&id) {
            violations.push(format!(
                "ids resolve: physical candidate {index} expands {id:?}, which Pass 2 pruned"
            ));
        }
    }
    let count = physical.physical_candidates.len();
    for (edge_id, edge) in physical.edges() {
        let (producer, consumer) = (edge.producer_candidate_id, edge.consumer_candidate_id);
        if producer.0 >= count || consumer.0 >= count {
            violations.push(format!("ids resolve: physical edge {edge_id:?} {edge:?}"));
            continue;
        }
        let consumer_logical = physical.physical_candidates[consumer.0].logical_candidate_id;
        let producer_logical = physical.physical_candidates[producer.0].logical_candidate_id;
        if !resolves(consumer_logical) || !resolves(producer_logical) {
            continue;
        }
        let input = memo
            .candidate(consumer_logical)
            .expression
            .operator
            .children()
            .get(edge.input_index_of_consumer)
            .copied()
            .copied();
        if input != Some(producer_logical.group) {
            violations.push(format!(
                "ids resolve: physical edge {edge_id:?} reads {:?}, not the consumer's input {input:?}",
                producer_logical.group
            ));
        }
    }
    violations
}

/// Every physical candidate's inputs have an admissible Stage 2 edge, from a
/// producer Stage 3 could select.
fn physical_inputs_have_producers(physical: &PhysicalASAPMemoCandidates) -> Vec<String> {
    let memo = &physical.logical_candidates.memo;
    let mut violations = Vec::new();
    for (index, candidate) in physical.physical_candidates.iter().enumerate() {
        let inputs = memo
            .candidate(candidate.logical_candidate_id)
            .expression
            .operator
            .children()
            .len();
        for input in 0..inputs {
            if physical
                .admissible_edges_into(PhysicalCandidateId(index), input)
                .is_empty()
            {
                violations.push(format!(
                    "every candidate's inputs exist: physical candidate {index} ({:?}) input \
                     {input} has no admissible edge",
                    candidate.logical_candidate_id
                ));
            }
        }
    }
    violations
}

/// The plan validates, each node is at its own index, reads nodes that
/// exist, and names the physical candidate it was built from, whose
/// operator kind it has.
fn plan_ids_resolve(plan: &PhysicalASAPDAG, physical: &PhysicalASAPMemoCandidates) -> Vec<String> {
    let mut violations = Vec::new();
    if let Err(error) = plan.validate() {
        violations.push(format!("ids resolve: the plan does not validate: {error}"));
    }
    let memo = &physical.logical_candidates.memo;
    for (index, node) in plan.nodes.iter().enumerate() {
        if node.id != index {
            violations.push(format!("ids resolve: node {} at index {index}", node.id));
        }
        for child in node.payload.children() {
            if *child >= plan.nodes.len() {
                violations.push(format!("ids resolve: node {index} reads node {child}"));
            }
        }
        match node.selected_physical_candidate {
            None => violations.push(format!(
                "ids resolve: node {index} names no physical candidate"
            )),
            Some(id) if id.0 >= physical.physical_candidates.len() => {
                violations.push(format!("ids resolve: node {index} names {id:?}"))
            }
            Some(id) => {
                let logical = physical.physical_candidates[id.0].logical_candidate_id;
                let operator = &memo.candidate(logical).expression.operator;
                if operator.kind_name() != node.payload.kind_name() {
                    violations.push(format!(
                        "ids resolve: node {index} is a {} built from {id:?}, a {}",
                        node.payload.kind_name(),
                        operator.kind_name()
                    ));
                }
            }
        }
    }
    for root in plan.roots.iter().chain(
        plan.scalar_roots
            .iter()
            .flat_map(|(_, e)| e.operator_refs()),
    ) {
        if *root >= plan.nodes.len() {
            violations.push(format!("ids resolve: root {root}"));
        }
    }
    violations
}

/// The physical candidate of plan node `node`; [`plan_ids_resolve`] has
/// checked it is set.
fn candidate_of(plan: &PhysicalASAPDAG, node: PhysicalASAPNodeId) -> PhysicalCandidateId {
    plan.nodes[node]
        .selected_physical_candidate
        .expect("checked by plan_ids_resolve")
}

/// Every plan edge (u, c, i) is a Stage 2 edge with α^log = α^phys = 1.
fn plan_edges_are_admissible(
    plan: &PhysicalASAPDAG,
    physical: &PhysicalASAPMemoCandidates,
) -> Vec<String> {
    let edges: BTreeMap<(usize, usize, usize), bool> = physical
        .edges()
        .map(|(_, edge)| {
            (
                (
                    edge.producer_candidate_id.0,
                    edge.consumer_candidate_id.0,
                    edge.input_index_of_consumer,
                ),
                edge.is_logically_and_physically_admissible(),
            )
        })
        .collect();
    let mut violations = Vec::new();
    for node in &plan.nodes {
        let consumer = candidate_of(plan, node.id);
        for (input, child) in node.payload.children().into_iter().enumerate() {
            let producer = candidate_of(plan, *child);
            match edges.get(&(producer.0, consumer.0, input)) {
                Some(true) => {}
                Some(false) => violations.push(format!(
                    "every selected edge is admissible: node {} input {input} reads node {child} \
                     over ({producer:?}, {consumer:?}, {input}), which has α = 0",
                    node.id
                )),
                None => violations.push(format!(
                    "every selected edge is admissible: node {} input {input} reads node {child} \
                     over ({producer:?}, {consumer:?}, {input}), which Stage 2 does not offer",
                    node.id
                )),
            }
        }
    }
    violations
}

/// The plan node (operator root) or nodes (a scalar root's operator
/// references) that answer each query, in workload order: query `q` is the
/// scalar root keyed `q` when there is one, else the next operator root.
fn query_answer_nodes(plan: &PhysicalASAPDAG, queries: usize) -> Vec<Vec<PhysicalASAPNodeId>> {
    let mut operator_roots = plan.roots.iter();
    (0..queries)
        .map(
            |q| match plan.scalar_roots.iter().find(|(key, _)| *key == q) {
                Some((_, expression)) => expression.operator_refs().into_iter().copied().collect(),
                None => operator_roots.next().copied().into_iter().collect(),
            },
        )
        .collect()
}

/// Every query has a plan root, built from a candidate of its root group
/// (or, for a scalar query, of each group its expression reads).
fn query_roots_are_served(
    plan: &PhysicalASAPDAG,
    physical: &PhysicalASAPMemoCandidates,
) -> Vec<String> {
    let memo = &physical.logical_candidates.memo;
    let mut violations = Vec::new();
    let answers = query_answer_nodes(plan, memo.queries.len());
    if plan.roots.len() + plan.scalar_roots.len() != memo.queries.len() {
        violations.push(format!(
            "every query root is served: {} roots and {} scalar roots for {} queries",
            plan.roots.len(),
            plan.scalar_roots.len(),
            memo.queries.len()
        ));
    }
    for (q, (query, nodes)) in memo.queries.iter().zip(&answers).enumerate() {
        let groups: Vec<MemoGroupId> = query.root.operator_refs().into_iter().copied().collect();
        let served: Vec<MemoGroupId> = nodes
            .iter()
            .map(|node| {
                physical.physical_candidates[candidate_of(plan, *node).0]
                    .logical_candidate_id
                    .group
            })
            .collect();
        if groups != served {
            violations.push(format!(
                "every query root is served: query {q} reads {groups:?}, the plan answers it with \
                 nodes {nodes:?} of {served:?}"
            ));
        }
    }
    violations
}

/// One plan node's composed error bound: per metric, and the failure
/// probability (a union bound over its readouts).
#[derive(Debug, Default, Clone)]
struct ComposedError {
    max_error_per_metric: Vec<(AccuracyMetric, f64)>,
    failure_probability: f64,
}

impl ComposedError {
    fn add(&mut self, metric: AccuracyMetric, error: f64) {
        match self
            .max_error_per_metric
            .iter_mut()
            .find(|(m, _)| *m == metric)
        {
            Some((_, total)) => *total += error,
            None => self.max_error_per_metric.push((metric, error)),
        }
    }
}

/// Constraint (K) on the plan (concepts doc, section 4, "Query
/// composition and root checks"), recomputed node by node from the leaves:
/// a node's error in metric m is its own readout requirement's (in m) plus
/// β_{c,i,m} times each input's; an input carrying error in a metric its
/// consumer has no contract for, or above the contract's validity cap,
/// violates (K); at each operator query root the error in the target's
/// metric is at most ε_q (every other metric 0; any metric when the target
/// names none) and, when the target sets a δ, the failure probability at
/// most the per-value δ_q. A scalar root accepts no error in any metric
/// and has no δ limit, as the MILP bounds it. Also returns how many
/// targeted queries have a nonzero composed error at their root.
fn constraint_k_holds(
    plan: &PhysicalASAPDAG,
    physical: &PhysicalASAPMemoCandidates,
) -> (Vec<String>, usize) {
    let memo = &physical.logical_candidates.memo;
    let mut violations = Vec::new();
    let mut composed: Vec<Option<ComposedError>> = vec![None; plan.nodes.len()];
    fn compose(
        node: PhysicalASAPNodeId,
        plan: &PhysicalASAPDAG,
        physical: &PhysicalASAPMemoCandidates,
        composed: &mut Vec<Option<ComposedError>>,
        violations: &mut Vec<String>,
    ) -> ComposedError {
        if let Some(done) = &composed[node] {
            return done.clone();
        }
        let memo = &physical.logical_candidates.memo;
        let logical = physical.physical_candidates[candidate_of(plan, node).0].logical_candidate_id;
        let candidate = memo.candidate(logical);
        let operator = &candidate.expression.operator;
        let mut error = ComposedError::default();
        // A readout's own error is its input's requirement; the MILP reads
        // one, so a second is a violation, not a sum.
        let mut own = candidate
            .input_requirements
            .iter()
            .filter_map(|requirement| requirement.required_accuracy);
        if let Some(required) = own.next() {
            error.add(required.metric, required.max_error);
            error.failure_probability += required.failure_probability;
        }
        if own.next().is_some() {
            violations.push(format!(
                "(K) holds: node {node} ({}) has more than one input with a required accuracy",
                operator.kind_name()
            ));
        }
        for (input, child) in plan.nodes[node].payload.children().into_iter().enumerate() {
            let below = compose(*child, plan, physical, composed, violations);
            error.failure_probability += below.failure_probability;
            for (metric, input_error) in below.max_error_per_metric {
                if input_error == 0.0 {
                    continue;
                }
                match LogicalRequirements::error_propagation(memo, operator, metric) {
                    None => violations.push(format!(
                        "(K) holds: node {node} ({}) reads error {input_error} in {metric:?} on \
                         input {input}, for which it has no error contract",
                        operator.kind_name()
                    )),
                    Some(propagation) => {
                        if let Some(Some(cap)) = propagation.validity_cap_per_input.get(input) {
                            if input_error > *cap {
                                violations.push(format!(
                                    "(K) holds: node {node} input {input} carries {input_error} in \
                                     {metric:?}, above its validity cap {cap}"
                                ));
                            }
                        }
                        let factor = propagation
                            .factor_per_input
                            .get(input)
                            .copied()
                            .unwrap_or(1.0);
                        error.add(metric, factor * input_error);
                    }
                }
            }
        }
        composed[node] = Some(error.clone());
        error
    }
    let answers = query_answer_nodes(plan, memo.queries.len());
    let mut queries_with_composed_error = 0;
    for (q, (query, nodes)) in memo.queries.iter().zip(&answers).enumerate() {
        let Some(target) = &query.accuracy_target else {
            continue;
        };
        let operator_root = !plan.scalar_roots.iter().any(|(key, _)| *key == q);
        // Within (K)'s solver tolerance.
        let slack = 1e-9;
        // No limit without a δ (nor at a scalar root); a whole-result δ
        // over an unknown number of values has none either (Pass 2 pruned
        // its readouts).
        let delta_per_value = target
            .failure_probability
            .and(target.failure_probability_per_value(query.values_per_evaluation))
            .filter(|_| operator_root);
        let mut composes_error = false;
        for node in nodes {
            let error = compose(*node, plan, physical, &mut composed, &mut violations);
            for (metric, bound) in &error.max_error_per_metric {
                composes_error |= *bound > 0.0;
                let allowed = match target.accuracy_metric {
                    _ if !operator_root => 0.0,
                    Some(target_metric) if target_metric != *metric => 0.0,
                    _ => target.max_error,
                };
                if *bound > allowed * (1.0 + slack) + slack * f64::EPSILON {
                    violations.push(format!(
                        "(K) holds: query {q}'s root node {node} has error {bound} in {metric:?}, \
                         its target allows {allowed} ({target:?})"
                    ));
                }
            }
            if let Some(delta_per_value) = delta_per_value {
                if error.failure_probability > delta_per_value * (1.0 + slack) {
                    violations.push(format!(
                        "(K) holds: query {q}'s root node {node} fails with probability {} per \
                         value, its target allows {delta_per_value}",
                        error.failure_probability
                    ));
                }
            }
        }
        queries_with_composed_error += usize::from(composes_error);
    }
    (violations, queries_with_composed_error)
}

/// Plans each of `workloads` to Stage 3, on every available core, and
/// panics with every contract violation, by workload id. With
/// `composes_error`, at least one targeted query must compose a nonzero
/// error at its root, so (K) is not checked over nothing.
fn assert_contracts_hold(workloads: Vec<CorpusWorkload>, composes_error: bool) {
    assert!(!workloads.is_empty(), "no workload checked");
    let failures = Mutex::new(Vec::new());
    let queries_with_composed_error = AtomicUsize::new(0);
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    std::thread::scope(|scope| {
        for _ in 0..threads.min(workloads.len()) {
            scope.spawn(|| {
                while let Some(workload) = workloads.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let found: Vec<String> = match workload.plan(RunUntil::Stage3) {
                        Err(error) => vec![format!("{}: does not plan: {error}", workload.id)],
                        Ok(planned) => {
                            let checked = checked_contracts(&planned, &workload.cost_inputs);
                            queries_with_composed_error
                                .fetch_add(checked.queries_with_composed_error, Ordering::Relaxed);
                            checked
                                .violations
                                .into_iter()
                                .map(|violation| format!("{}: {violation}", workload.id))
                                .collect()
                        }
                    };
                    failures.lock().unwrap().extend(found);
                }
            });
        }
    });
    let mut failures = failures.into_inner().unwrap();
    failures.sort();
    assert!(
        failures.is_empty(),
        "{} contract violation(s) over {} workloads:\n{}",
        failures.len(),
        workloads.len(),
        failures.join("\n")
    );
    if composes_error {
        assert!(
            queries_with_composed_error.into_inner() > 0,
            "no targeted query composes a nonzero error: (K) checked nothing"
        );
    }
}

/// The registry's workloads of `source` (one of `corpus_registry::SOURCES`).
fn registry(source: &str) -> Vec<CorpusWorkload> {
    assert!(
        SOURCES.contains(&source),
        "{source} is not a registry source"
    );
    let prefix = format!("{source}/");
    corpus_workloads()
        .unwrap()
        .into_iter()
        .filter(|workload| workload.id.starts_with(&prefix))
        .collect()
}

/// Every o11y benchmark query (14, single queries, range evaluations,
/// offsets, subqueries) meets the contracts. Their plans are exact (no
/// query composes an error), so the nonzero-error check is on the sketch
/// plan workloads.
#[test]
fn every_o11y_bench_workload_meets_the_contracts() {
    assert_contracts_hold(registry("o11y_bench"), false);
}

/// Every sketch plan snapshot workload (MVP and raw-retained deployments,
/// sketches with accuracy targets) meets the contracts.
#[test]
fn every_sketch_plan_workload_meets_the_contracts() {
    assert_contracts_hold(registry("sketch_plans"), true);
}

/// The planner-layering examples 1, 2, 3a and 3b meet the contracts.
#[test]
fn every_planner_layering_example_meets_the_contracts() {
    assert_contracts_hold(registry("planner_layering"), false);
}

/// The viewer's PromQL and SQL memo examples meet the contracts.
#[test]
fn every_memo_example_meets_the_contracts() {
    assert_contracts_hold(registry("memo_examples"), false);
}

/// The `planner_fuzz` seeds, accuracy not enforced, meet the contracts.
#[test]
fn every_fuzz_seed_meets_the_contracts() {
    assert_contracts_hold(registry("fuzz_seeds"), false);
}

/// Every registry source is checked by one of the tests here: a new
/// source needs its own test.
#[test]
fn every_registry_source_has_a_contracts_test() {
    let mut checked = [
        "o11y_bench",
        "sketch_plans",
        "planner_layering",
        "memo_examples",
        "fuzz_seeds",
        "random_mixed",
    ];
    let mut sources = SOURCES;
    checked.sort();
    sources.sort();
    assert_eq!(checked, sources);
}

/// The registry workload `id` planned to Stage 3, with its cost inputs.
fn planned(id: &str) -> (PlannedWorkload, CostInputs) {
    let workload = corpus_workloads()
        .unwrap()
        .into_iter()
        .find(|workload| workload.id == id)
        .unwrap();
    (
        workload.plan(RunUntil::Stage3).unwrap(),
        workload.cost_inputs,
    )
}

/// The sketch plan workload `ratio-of-quantiles-1h-mvp` (q0.9 / q0.5 over
/// 1 h, ε = 0.05, no raw data kept) with its failure probability removed:
/// the two DDSketch readouts compose δ = 2, which no limit bounds without
/// a target δ (the MILP sets none), so the plan meets the contracts.
#[test]
fn a_target_without_a_failure_probability_bounds_no_failure_probability() {
    let mut workload = corpus_workloads()
        .unwrap()
        .into_iter()
        .find(|workload| workload.id == "sketch_plans/ratio-of-quantiles-1h-mvp")
        .unwrap();
    for entry in workload
        .workload
        .query_workload
        .repeating_queries
        .iter_mut()
        .flatten()
    {
        entry.requirements.accuracy.failure_probability = None;
    }
    let planned = workload.plan(RunUntil::Stage3).unwrap();
    let checked = checked_contracts(&planned, &workload.cost_inputs);
    assert!(checked.violations.is_empty(), "{:#?}", checked.violations);
    assert_eq!(checked.queries_with_composed_error, 1);
}

/// The ratio query's accuracy targets in `planned`, for tampering.
fn targets(planned: &mut PlannedWorkload) -> impl Iterator<Item = &mut AccuracyTarget> {
    planned
        .stages
        .run
        .physical_asap_candidates
        .as_mut()
        .unwrap()
        .logical_candidates
        .memo
        .queries
        .iter_mut()
        .filter_map(|query| query.accuracy_target.as_mut())
}

/// The contracts are not vacuous. On the ratio of two quantiles (ε = 0.05
/// relative at its root, δ = 0.001), (K) is violated by a target tightened
/// to ε = 1e-9, by a target naming the rank metric, and by δ = 1e-12; a
/// selected edge marked not logically admissible is caught; an objective
/// 0.1 % above `plan_cost` is caught; and swapping the plan's answers to
/// two queries leaves neither served.
#[test]
fn a_tampered_target_edge_objective_or_root_violates_the_contracts() {
    let has = |violations: &[String], prefix: &str, part: &str| {
        violations
            .iter()
            .any(|v| v.starts_with(prefix) && v.contains(part))
    };

    let (mut ratio, ratio_costs) = planned("sketch_plans/ratio-of-quantiles-1h-mvp");
    assert!(contract_violations(&ratio, &ratio_costs).is_empty());
    let original: Vec<AccuracyTarget> = targets(&mut ratio).map(|target| target.clone()).collect();
    let restore = |ratio: &mut PlannedWorkload| {
        for (target, original) in targets(ratio).zip(&original) {
            *target = original.clone();
        }
    };
    targets(&mut ratio).for_each(|target| target.max_error = 1e-9);
    assert!(has(
        &contract_violations(&ratio, &ratio_costs),
        "(K) holds",
        "has error"
    ));
    restore(&mut ratio);
    targets(&mut ratio).for_each(|target| target.accuracy_metric = Some(AccuracyMetric::Rank));
    assert!(has(
        &contract_violations(&ratio, &ratio_costs),
        "(K) holds",
        "its target allows 0"
    ));
    restore(&mut ratio);
    targets(&mut ratio).for_each(|target| target.failure_probability = Some(1e-12));
    assert!(has(
        &contract_violations(&ratio, &ratio_costs),
        "(K) holds",
        "fails with probability"
    ));
    restore(&mut ratio);

    // Stage 2's output again, with the edge into the plan's first node
    // that reads another marked not logically admissible.
    let (plan, _) = ratio.stages.run.selection.as_ref().unwrap();
    let consumer = plan
        .nodes
        .iter()
        .find(|node| !node.payload.children().is_empty())
        .unwrap();
    let selected_edge = (
        plan.nodes[*consumer.payload.children()[0]]
            .selected_physical_candidate
            .unwrap(),
        consumer.selected_physical_candidate.unwrap(),
    );
    let physical = ratio.stages.run.physical_asap_candidates.take().unwrap();
    let edges = physical
        .edges()
        .map(|(_, mut edge)| {
            if (edge.producer_candidate_id, edge.consumer_candidate_id) == selected_edge
                && edge.input_index_of_consumer == 0
            {
                edge.satisfies_logical_requirement = false;
            }
            edge
        })
        .collect();
    ratio.stages.run.physical_asap_candidates = Some(PhysicalASAPMemoCandidates::new(
        physical.logical_candidates,
        physical.physical_candidates,
        edges,
        physical.pruned_candidates,
    ));
    let violations = contract_violations(&ratio, &ratio_costs);
    assert!(has(
        &violations,
        "every selected edge is admissible",
        "α = 0"
    ));
    assert!(has(
        &violations,
        "every candidate's inputs exist",
        "no admissible edge"
    ));

    let (mut objective, objective_costs) = planned("o11y_bench/q21");
    objective
        .stages
        .run
        .selection
        .as_mut()
        .unwrap()
        .1
        .milp_objective *= 1.001;
    assert!(has(
        &contract_violations(&objective, &objective_costs),
        "MILP objective = plan_cost",
        ""
    ));

    let (mut swapped, swapped_costs) = planned("planner_layering/example1");
    let roots = &mut swapped.stages.run.selection.as_mut().unwrap().0.roots;
    assert_eq!(roots.len(), 2);
    roots.swap(0, 1);
    assert!(has(
        &contract_violations(&swapped, &swapped_costs),
        "every query root is served",
        ""
    ));
}

mod full_tier {
    use super::*;

    /// The 30 random mixed workloads (nine PromQL queries each over 10,000
    /// series, AWS Fargate costs, no raw data) meet the contracts.
    #[test]
    #[ignore = "full tier: 30 workloads at about 7 s each in a debug build"]
    fn every_random_mixed_workload_meets_the_contracts() {
        assert_contracts_hold(registry("random_mixed"), false);
    }
}
