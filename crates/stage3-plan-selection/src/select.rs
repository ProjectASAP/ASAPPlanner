//! Stage 3, plan selection: [`select_plan`] lists the workload's execution
//! contexts with each execution and edge they allow
//! ([`crate::milp::execution_contexts`]), costs every execution
//! ([`crate::execution_cost`]), builds and solves the selection MILP over
//! them ([`crate::milp::model`]), decodes $\{x = 1\}$, $\{y = 1\}$ and
//! $\{z = 1\}$ into a
//! [`DagOfSelectedCandidates`](asap_types::stages::DagOfSelectedCandidates),
//! and realizes it once through the injected realizer.

use std::collections::BTreeMap;

use asap_accuracy::AccuracyModel;
use asap_cost::analytical::AnalyticalCostError;
use asap_cost::execution_demand::evaluation_rate;
use asap_cost::{materialized_tumbling_window_chains, node_map, plan_cost, CostInputs, CostModel};
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, QueryRoot};
use asap_types::physical::{ExecutionTiming, Lifetime, StorageMedium};
use asap_types::stages::{
    AssignmentRealizer, ExecutionContext, ExecutionContextId, PhysicalASAPMemoCandidates,
    PhysicalCandidateError, PhysicalCandidateId, PruneReason, PrunedCandidate, RealizedPlan,
};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::execution_cost::{execution_costs, ExecutionCosts};
use crate::milp::execution_contexts::{Execution, ExecutionContexts};
use crate::milp::model::{solve_selection, SelectionInputs, SelectionMilpError, SolvedSelection};
use crate::models::AccuracyCheckInputs;
use crate::report::{
    AcceptedAccuracy, ErrorSelectingPlan, InexactObjective, Rejection, RejectionCause,
    SearchReport, SearchStatus, Selection,
};

/// Relative difference under which the MILP's objective and `plan_cost`
/// of the selected plan count as equal.
const COST_EQUALITY_TOLERANCE: f64 = 1e-6;

/// The terms of `asap_cost::plan_cost` the formulation's objective (concepts
/// doc, section 5) does not encode, named in [`InexactObjective`] with the
/// number of selected executions each may concern.
const HELD_MEMORY: &str = "memory a query-time result holds while its evaluation runs: \
     plan_cost holds it for the latency of the queries reading it; the objective ignores query \
     latency (out of scope)";
const RUNS_PER_QUERY: &str = "a not-materialized execution in a context of several queries: \
     plan_cost computes it once per query reaching it, the objective once per context";
const CHAIN_BUILDS_PER_REQUEST: &str = "a tumbling-window chain materialized at query time: \
     plan_cost builds it once per request of the queries reaching it, the objective once per \
     window";
const CHAIN_MERGE: &str = "the merge the realizer reads a tumbling-window chain materialized \
     across evaluations through: plan_cost runs it per query reading it; it is not a candidate";
const REPRESENTATIVE_PRODUCER: &str = "a consumer costed over the output of another logical \
     candidate of its input than the selected one: the objective costs each execution over one \
     representative producer per input";
const RANGE_UNDER_CHAIN: &str = "a range read only by tumbling-window chains run in its \
     context: the realizer builds each chain over the range's input, so plan_cost has no range";
const QUERY_TIME_SCAN: &str = "a query-time scan: plan_cost reads the windows of its selected \
     consumers and retains the raw data of the largest scan of each source; the objective reads \
     the windows of every consumer that may read it in its context and retains each scan's";

/// Stage 3, plan selection: select one plan among `physical`'s candidates
/// (module documentation). The selection is realized through `realizer`
/// and costed under `cost_inputs` (cost model 1 only). `entries[q]` is
/// query `q`'s workload entry; `data` the data workload with its bounds.
///
/// Accuracy: until constraint (K) (#1207 PR 8), a query's accuracy rests on
/// the parameter floors of its edges, which Pass 2 derived from the query's
/// target ((G): an edge from a producer below its floor is not admissible);
/// `accuracy_checks.model` may accept an output regardless of its
/// guarantee, which the report records.
///
/// Err: invalid or unsupported cost inputs, an infeasible catalog, a
/// solver failure, or the realizer rejected the selected plan
/// ([`ErrorSelectingPlan::RealizerRejected`]: a Stage 2 catalog bug).
pub fn select_plan(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    accuracy_checks: &AccuracyCheckInputs<'_>,
    cost_inputs: &CostInputs,
    realizer: &dyn AssignmentRealizer,
) -> Result<Selection, ErrorSelectingPlan> {
    cost_inputs
        .validate()
        .map_err(ErrorSelectingPlan::InvalidSettings)?;
    if !matches!(cost_inputs.cost_model, CostModel::UsageOverTime(_)) {
        return Err(ErrorSelectingPlan::UnsupportedCostModel(
            "cost models 2 and 3 (the selection MILP implements cost model 1 only)".into(),
        ));
    }
    // The MILP's coefficients and `plan_cost` must use one calibration: a
    // measured cost table needs a frozen factor.
    if cost_inputs.frozen_calibration_factor.is_none()
        && !cost_inputs.measured_operation_costs.operators.is_empty()
    {
        return Err(ErrorSelectingPlan::UnsupportedCostModel(
            "a measured operation cost table without a frozen calibration factor".into(),
        ));
    }
    if entries.len() != physical.query_roots.len() {
        return Err(ErrorSelectingPlan::EntryCount {
            candidate: "the physical memo".into(),
            roots: physical.query_roots.len(),
            entries: entries.len(),
        });
    }
    let formula_scale = cost_inputs.frozen_calibration_factor.unwrap_or(1.0);
    let contexts = ExecutionContexts::new(physical, entries);
    let costs = execution_costs(physical, &contexts, data, cost_inputs, formula_scale);
    let pruned_candidates = uncostable_candidates(physical, &contexts, &costs);
    let solved =
        solve_selection(&SelectionInputs::new(physical, &contexts, &costs)).map_err(|error| {
            match error {
                SelectionMilpError::Infeasible => {
                    ErrorSelectingPlan::NoValidCandidate(vec![infeasible(&costs)])
                }
                SelectionMilpError::Solver(reason) => ErrorSelectingPlan::Solver(reason),
            }
        })?;
    let validation_started = std::time::Instant::now();
    solved.selected_dag.validate(physical).map_err(|reason| {
        ErrorSelectingPlan::Solver(format!("the decoded selection is invalid: {reason}"))
    })?;
    let realized =
        realizer
            .realize(physical, &solved.selected_dag)
            .map_err(|error| match error {
                PhysicalCandidateError::RejectedAssignment(reason) => {
                    ErrorSelectingPlan::RealizerRejected(reason)
                }
                error => ErrorSelectingPlan::RealizerRejected(error.to_string()),
            })?;
    let plan = &realized.plan;
    let recurrence: Vec<_> = entries.iter().map(|e| e.recurrence.clone()).collect();
    let ranges: Vec<_> = entries.iter().map(|e| e.range_evaluation).collect();
    let (cost, _) = plan_cost(
        &plan.dag,
        &node_map(&plan.dag),
        &materialized_tumbling_window_chains(&plan.dag),
        &recurrence,
        &ranges,
        data,
        cost_inputs,
        formula_scale,
    )
    .map_err(|(node, error)| ErrorSelectingPlan::Cost {
        candidate: plan.id.clone(),
        node,
        error,
    })?;
    let plan_cost_per_second = cost.cost_per_second.ok_or_else(|| {
        ErrorSelectingPlan::UnsupportedCostModel("a cost model without a cost per second".into())
    })?;
    // The runtime's fee per query request: the same for every plan, so a
    // constant of the objective.
    let objective_per_second = solved.objective_per_second
        + request_cost_per_second(entries, cost_inputs).map_err(|error| {
            ErrorSelectingPlan::Cost {
                candidate: plan.id.clone(),
                node: plan.dag.nodes.first().map_or(0, |node| node.id),
                error,
            }
        })?;
    let difference = (objective_per_second - plan_cost_per_second).abs();
    let tolerance = COST_EQUALITY_TOLERANCE * plan_cost_per_second.abs().max(f64::MIN_POSITIVE);
    let inexact_objective = match difference > tolerance {
        false => None,
        true => {
            let inexact_terms = inexact_terms(physical, &contexts, &costs, cost_inputs, &solved);
            // A difference none of the formulation's known gaps explains is
            // a bug in a cost coefficient.
            if inexact_terms.is_empty() {
                return Err(ErrorSelectingPlan::ObjectiveDiffersFromPlanCost {
                    objective_per_second,
                    plan_cost_per_second,
                });
            }
            Some(InexactObjective {
                objective_per_second,
                plan_cost_per_second,
                inexact_terms,
            })
        }
    };
    let accepted_accuracy_per_query = accepted_accuracy(&realized, entries, accuracy_checks.model);
    let validation_ms = validation_started.elapsed().as_millis() as u64;
    Ok(Selection {
        selected_dag: solved.selected_dag,
        cost,
        accepted_accuracy_per_query,
        pruned_candidates,
        search_report: SearchReport {
            status: SearchStatus::Optimal,
            // The solver's bound is on the MILP's objective; it bounds the
            // plan's cost only when the objective equals it.
            cost_lower_bound_per_second: (inexact_objective.is_none()
                && solved.lower_bound_per_second.is_finite())
            .then_some(
                solved.lower_bound_per_second
                    + (objective_per_second - solved.objective_per_second),
            ),
            milp_variables: solved.variable_count,
            milp_constraints: solved.constraint_count,
            model_construction_ms: solved.construction_ms,
            solver_ms: solved.solver_ms,
            validation_ms,
            inexact_objective,
        },
    })
}

/// The rejection of an infeasible selection: no plan meets (A)–(I) and
/// (R), with the reasons `asap-cost` could not cost the executions the
/// model left out, each once in execution order: those executions may be
/// why no plan is left.
fn infeasible(costs: &ExecutionCosts) -> Rejection {
    let mut reasons: Vec<&str> = Vec::new();
    for reason in costs.uncostable.values() {
        if !reasons.contains(&reason.as_str()) {
            reasons.push(reason);
        }
    }
    let mut reason = "no plan gives every query output a candidate whose every input has an \
                      admissible producer (the selection MILP is infeasible)"
        .to_string();
    if !reasons.is_empty() {
        reason += &format!(
            "; executions left out because they could not be costed: {}",
            reasons.join("; ")
        );
    }
    Rejection {
        id: "workload".into(),
        cause: RejectionCause::Infeasible,
        reason,
    }
}

/// The candidates Stage 3 rules out before the search: those with an
/// execution none of which `asap-cost` could cost, in candidate order.
fn uncostable_candidates(
    physical: &PhysicalASAPMemoCandidates,
    contexts: &ExecutionContexts,
    costs: &ExecutionCosts,
) -> Vec<PrunedCandidate<PhysicalCandidateId>> {
    let mut costed: BTreeMap<PhysicalCandidateId, bool> = BTreeMap::new();
    for &execution in contexts.executions() {
        *costed.entry(execution.candidate()).or_default() |=
            costs.cost_per_second.contains_key(&execution);
    }
    costed
        .into_iter()
        .filter(|&(_, costed)| !costed)
        .map(|(candidate, _)| PrunedCandidate {
            group: physical.physical_candidates[candidate.0]
                .logical_candidate_id
                .group,
            candidate,
            reason: PruneReason::UsageNotCostable,
        })
        .collect()
}

/// The runtime's fee per query request under cost model 1, per second, as
/// `asap_cost::plan_cost` charges it: `ns_per_evaluation` of the measured
/// table, once per request of each query.
fn request_cost_per_second(
    entries: &[QueryWorkloadEntry],
    cost_inputs: &CostInputs,
) -> Result<f64, AnalyticalCostError> {
    let evaluation_ms = cost_inputs.measured_operation_costs.ns_per_evaluation * 1e-6;
    let CostModel::UsageOverTime(unit_cost) = &cost_inputs.cost_model else {
        return Ok(0.0);
    };
    if evaluation_ms <= 0.0 {
        return Ok(0.0);
    }
    let mut cost = 0.0;
    for entry in entries {
        let requests_per_second = evaluation_rate(
            [(&entry.recurrence, None)],
            cost_inputs.non_repeating_query_horizon_seconds,
        )?;
        cost += unit_cost.cost_per_cpu_op * evaluation_ms
            / cost_inputs.resource_speed.ms_per_cpu_op
            * requests_per_second;
    }
    Ok(cost)
}

/// The terms of `plan_cost` the objective does not encode that the
/// selected executions may meet, each with how many of them: query-time
/// results holding memory (when memory has a cost), not-materialized executions in a context of
/// several queries, query-time materialized chains, chains materialized
/// across evaluations read through the realizer's merge, consumers costed
/// over another producer than the selected one, ranges read only by
/// chains, query-time scans.
fn inexact_terms(
    physical: &PhysicalASAPMemoCandidates,
    contexts: &ExecutionContexts,
    costs: &ExecutionCosts,
    cost_inputs: &CostInputs,
    solved: &SolvedSelection,
) -> BTreeMap<String, usize> {
    let memo = &physical.logical_candidates.memo;
    let memory_costs = matches!(
        &cost_inputs.cost_model,
        CostModel::UsageOverTime(unit_cost) if unit_cost.cost_per_memory_byte_second > 0.0
    );
    let execution_of_node = &solved.execution_of_node;
    let windowed = |node: usize| {
        memo.candidate(
            physical.physical_candidates[execution_of_node[node].candidate().0]
                .logical_candidate_id,
        )
        .logical_properties
        .window_layout()
        .is_some()
    };
    let mut terms: BTreeMap<String, usize> = BTreeMap::new();
    // Per node, whether every consumer selected for it is a chain.
    let mut read_only_by_chains: BTreeMap<usize, bool> = BTreeMap::new();
    for edge in &solved.selected_dag.edges {
        let (producer, consumer) = (edge.producer.0, edge.consumer.0);
        *read_only_by_chains.entry(producer).or_insert(true) &= windowed(consumer);
        let representative = costs
            .representative_producers
            .get(&execution_of_node[consumer])
            .and_then(|producers| producers.get(edge.input_index_of_consumer));
        // Another run of the same logical candidate at the same timing has
        // the same output.
        let same_output = |a: Execution, b: Execution| {
            let (a, b) = (
                &physical.physical_candidates[a.candidate().0],
                &physical.physical_candidates[b.candidate().0],
            );
            a.logical_candidate_id == b.logical_candidate_id
                && a.physical_properties.timing() == b.physical_properties.timing()
        };
        if !representative.is_some_and(|&r| same_output(r, execution_of_node[producer])) {
            *terms
                .entry(REPRESENTATIVE_PRODUCER.to_string())
                .or_default() += 1;
        }
    }
    for (node, only_chains) in read_only_by_chains {
        let candidate = &physical.physical_candidates[execution_of_node[node].candidate().0];
        if only_chains
            && matches!(
                memo.candidate(candidate.logical_candidate_id)
                    .expression
                    .operator,
                Operator::NonASAP(NonASAPOp::TimeRange { .. })
            )
            && matches!(
                execution_of_node[node],
                Execution::InContext(context, _)
                    if matches!(contexts.contexts()[context.0], ExecutionContext::Evaluation { .. })
            )
        {
            *terms.entry(RANGE_UNDER_CHAIN.to_string()).or_default() += 1;
        }
    }
    let queries_of_context = |context: ExecutionContextId| {
        (0..physical.query_roots.len())
            .filter(|&query| contexts.context_of_query(query) == Some(context))
            .count()
    };
    for &execution in execution_of_node {
        let candidate = &physical.physical_candidates[execution.candidate().0];
        let properties = candidate.physical_properties;
        let logical = memo.candidate(candidate.logical_candidate_id);
        let query_time = properties.timing() == ExecutionTiming::QueryTime;
        let mut add = |term: &str| *terms.entry(term.to_string()).or_default() += 1;
        if memory_costs
            && query_time
            && matches!(
                properties.lifetime(),
                Lifetime::None | Lifetime::OneEvaluation(StorageMedium::Memory)
            )
        {
            add(HELD_MEMORY);
        }
        if let Execution::InContext(context, _) = execution {
            if properties.lifetime() == Lifetime::None && queries_of_context(context) > 1 {
                add(RUNS_PER_QUERY);
            }
        }
        if query_time && matches!(properties.lifetime(), Lifetime::AcrossEvaluations(_)) {
            add(CHAIN_BUILDS_PER_REQUEST);
        }
        if matches!(properties.lifetime(), Lifetime::AcrossEvaluations(_)) {
            add(CHAIN_MERGE);
        }
        if query_time
            && matches!(
                logical.expression.operator,
                Operator::NonASAP(NonASAPOp::Scan { .. })
            )
        {
            add(QUERY_TIME_SCAN);
        }
    }
    terms
}

/// Per query, the accuracy accepted for the selected plan. An output that
/// reads no sketch is exact. Otherwise, until constraint (K) (#1207 PR 8)
/// checks the composed bound at the root, the output is accepted in its
/// target's metric through the parameter floors of its edges (Pass 2's
/// metric rule admits only edges whose metric converts to it), unless the
/// deployment's model accepts it regardless of its guarantee.
fn accepted_accuracy(
    realized: &RealizedPlan,
    entries: &[QueryWorkloadEntry],
    model: &dyn AccuracyModel,
) -> Vec<AcceptedAccuracy> {
    realized
        .plan
        .roots
        .iter()
        .zip(entries)
        .map(|(root, entry)| {
            let target = &entry.requirements.accuracy;
            let sketches = sketches_read(root);
            let exact = sketches.is_empty();
            let regardless = !exact && model.accepts_output_regardless_of_guarantee(root, target);
            AcceptedAccuracy {
                target_metric: target.accuracy_metric,
                accepted_metric: (!exact && !regardless)
                    .then_some(target.accuracy_metric)
                    .flatten(),
                accepted_regardless_of_guarantee: regardless,
                guarantee_check_rejection: None,
                sketches,
            }
        })
        .collect()
}

/// The sketch algorithms of the summary builds `root` reads, in plan order
/// (depth first, inputs in order), each build once.
fn sketches_read(root: &QueryRoot) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut sketches = Vec::new();
    let mut pending: Vec<std::rc::Rc<OperatorNode>> = match root {
        QueryRoot::Operator(node) => vec![std::rc::Rc::clone(node)],
        QueryRoot::Scalar(expression) => expression
            .operator_refs()
            .into_iter()
            .rev()
            .cloned()
            .collect(),
    };
    while let Some(node) = pending.pop() {
        if !seen.insert(std::rc::Rc::as_ptr(&node)) {
            continue;
        }
        if let Some(
            ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(algorithm, _),
                ..
            }
            | ASAPOp::SummaryTumblingWindowChain {
                family: FieldDataType::Sketch(algorithm, _),
                ..
            },
        ) = node.asap()
        {
            sketches.push(format!("{algorithm:?}"));
        }
        pending.extend(node.children().into_iter().rev().cloned());
    }
    sketches
}
