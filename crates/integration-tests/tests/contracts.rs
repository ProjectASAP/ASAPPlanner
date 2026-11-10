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

use std::collections::{BTreeMap, BTreeSet};

use asap_devtools::corpus_registry::{corpus_workloads, CorpusWorkload, PlannedWorkload};
use asap_devtools::planning_stages::RunUntil;
use asap_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPNodeId};
use asap_types::ir::properties::AccuracyMetric;
use asap_types::stages::{
    CandidateForMemoGroupId, LogicalRequirements, Memo, MemoGroupId, PhysicalASAPMemoCandidates,
    PhysicalCandidateId,
};

/// The relative tolerance of MILP objective = `plan_cost`.
const OBJECTIVE_RELATIVE_TOLERANCE: f64 = 1e-9;

/// Every contract violation of `planned`, each a line naming the
/// invariant; empty when every contract holds. A workload Stage 3 selects
/// no plan for violates "every query root is served".
pub fn contract_violations(planned: &PlannedWorkload) -> Vec<String> {
    let run = &planned.stages.run;
    let (Some(logical), Some(physical)) =
        (&run.logical_asap_candidates, &run.physical_asap_candidates)
    else {
        return vec!["every query root is served: Pass 2 or Stage 2 produced nothing".into()];
    };
    let memo = &logical.memo;
    let mut violations = memo_ids_resolve(memo, logical);
    violations.extend(physical_ids_resolve(physical));
    let (plan, selection) = match &run.selection {
        Ok(selected) => selected,
        Err(error) => {
            violations.push(format!("every query root is served: no plan ({error})"));
            return violations;
        }
    };
    violations.extend(plan_ids_resolve(plan, physical));
    if !violations.is_empty() {
        // The checks below index by these ids.
        return violations;
    }
    violations.extend(plan_edges_are_admissible(plan, physical));
    violations.extend(query_roots_are_served(plan, physical));
    match asap_cost::plan_cost(plan, &planned.entries, &planned.data, &planned.cost_inputs) {
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
    violations.extend(constraint_k_holds(plan, physical));
    violations
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
/// violates (K); at each query root the error in the target's metric is at
/// most ε_q (every other metric 0; any metric when the target names none)
/// and the failure probability at most the per-value δ_q.
fn constraint_k_holds(
    plan: &PhysicalASAPDAG,
    physical: &PhysicalASAPMemoCandidates,
) -> Vec<String> {
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
        // A readout's own error is its input's requirement.
        for requirement in &candidate.input_requirements {
            if let Some(required) = requirement.required_accuracy {
                error.add(required.metric, required.max_error);
                error.failure_probability += required.failure_probability;
            }
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
    for (q, (query, nodes)) in memo.queries.iter().zip(&answers).enumerate() {
        let Some(target) = &query.accuracy_target else {
            continue;
        };
        // Within (K)'s solver tolerance.
        let slack = 1e-9;
        let delta_per_value = target
            .failure_probability_per_value(query.values_per_evaluation)
            .unwrap_or(0.0);
        for node in nodes {
            let error = compose(*node, plan, physical, &mut composed, &mut violations);
            for (metric, bound) in &error.max_error_per_metric {
                let allowed = match target.accuracy_metric {
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
            if error.failure_probability > delta_per_value * (1.0 + slack) {
                violations.push(format!(
                    "(K) holds: query {q}'s root node {node} fails with probability {} per value, \
                     its target allows {delta_per_value}",
                    error.failure_probability
                ));
            }
        }
    }
    violations
}

/// Plans each of `workloads` to Stage 3 and panics with every contract
/// violation, by workload id.
fn assert_contracts_hold(workloads: impl IntoIterator<Item = CorpusWorkload>) {
    let mut failures = Vec::new();
    let mut checked = 0;
    for workload in workloads {
        checked += 1;
        match workload.plan(RunUntil::Stage3) {
            Err(error) => failures.push(format!("{}: does not plan: {error}", workload.id)),
            Ok(planned) => failures.extend(
                contract_violations(&planned)
                    .into_iter()
                    .map(|violation| format!("{}: {violation}", workload.id)),
            ),
        }
    }
    assert!(checked > 0, "no workload checked");
    assert!(
        failures.is_empty(),
        "{} contract violation(s) over {checked} workloads:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The registry's workloads whose id starts with `prefix`.
fn registry(prefix: &str) -> Vec<CorpusWorkload> {
    corpus_workloads()
        .unwrap()
        .into_iter()
        .filter(|workload| workload.id.starts_with(prefix))
        .collect()
}

/// Every o11y benchmark query (14, single queries, range evaluations,
/// offsets, subqueries) meets the contracts.
#[test]
fn every_o11y_bench_workload_meets_the_contracts() {
    assert_contracts_hold(registry("o11y_bench/"));
}

/// Every sketch plan snapshot workload (MVP and raw-retained deployments,
/// sketches with accuracy targets) meets the contracts.
#[test]
fn every_sketch_plan_workload_meets_the_contracts() {
    assert_contracts_hold(registry("sketch_plans/"));
}

/// The planner-layering examples 1, 2, 3a and 3b meet the contracts.
#[test]
fn every_planner_layering_example_meets_the_contracts() {
    assert_contracts_hold(registry("planner_layering/"));
}

/// The viewer's PromQL and SQL memo examples meet the contracts.
#[test]
fn every_memo_example_meets_the_contracts() {
    assert_contracts_hold(registry("memo_examples/"));
}

/// The `planner_fuzz` seeds, accuracy not enforced, meet the contracts.
#[test]
fn every_fuzz_seed_meets_the_contracts() {
    assert_contracts_hold(registry("fuzz_seeds/"));
}

/// Every registry source is checked by one of the tests here: a new
/// source needs its own test.
#[test]
fn every_registry_source_has_a_contracts_test() {
    let checked = [
        "o11y_bench/",
        "sketch_plans/",
        "planner_layering/",
        "memo_examples/",
        "fuzz_seeds/",
        "random_mixed/",
    ];
    for workload in corpus_workloads().unwrap() {
        assert!(
            checked.iter().any(|prefix| workload.id.starts_with(prefix)),
            "{} is in no contracts test",
            workload.id
        );
    }
}

/// The registry workload `id` planned to Stage 3.
fn planned(id: &str) -> PlannedWorkload {
    corpus_workloads()
        .unwrap()
        .into_iter()
        .find(|workload| workload.id == id)
        .unwrap()
        .plan(RunUntil::Stage3)
        .unwrap()
}

/// The contracts are not vacuous: (K) composes a nonzero error for the
/// ratio of two quantiles (ε = 0.05 at its root), so a
/// target tightened to 1e-9 is violated; an objective 0.1 % above
/// `plan_cost` is caught; and swapping the plan's answers to two queries
/// leaves neither served.
#[test]
fn a_tampered_target_objective_or_root_violates_the_contracts() {
    let has =
        |violations: &[String], prefix: &str| violations.iter().any(|v| v.starts_with(prefix));

    let mut tightened = planned("sketch_plans/ratio-of-quantiles-1h-mvp");
    assert!(contract_violations(&tightened).is_empty());
    let physical = tightened
        .stages
        .run
        .physical_asap_candidates
        .as_mut()
        .unwrap();
    for query in &mut physical.logical_candidates.memo.queries {
        if let Some(target) = query.accuracy_target.as_mut() {
            target.max_error = 1e-9;
        }
    }
    assert!(has(&contract_violations(&tightened), "(K) holds"));

    let mut objective = planned("o11y_bench/q21");
    objective
        .stages
        .run
        .selection
        .as_mut()
        .unwrap()
        .1
        .milp_objective *= 1.001;
    assert!(has(
        &contract_violations(&objective),
        "MILP objective = plan_cost"
    ));

    let mut swapped = planned("planner_layering/example1");
    let roots = &mut swapped.stages.run.selection.as_mut().unwrap().0.roots;
    assert_eq!(roots.len(), 2);
    roots.swap(0, 1);
    assert!(has(
        &contract_violations(&swapped),
        "every query root is served"
    ));
}

mod full_tier {
    use super::*;

    /// The 30 random mixed workloads (nine PromQL queries each over 10,000
    /// series, AWS Fargate costs, no raw data) meet the contracts.
    #[test]
    #[ignore = "full tier: 30 workloads at about 7 s each in a debug build"]
    fn every_random_mixed_workload_meets_the_contracts() {
        assert_contracts_hold(registry("random_mixed/"));
    }
}
