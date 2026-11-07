//! Stage 3, plan selection: [`select_plan`] lists the workload's execution
//! contexts with each execution and edge they allow
//! ([`crate::milp::execution_contexts`]), costs every execution
//! ([`crate::execution_cost`]), builds and solves the selection MILP over
//! them ([`crate::milp::model`]), decodes $\{x = 1\}$, $\{y = 1\}$ and
//! $\{z = 1\}$ into a
//! [`DagOfSelectedCandidates`](asap_types::stages::DagOfSelectedCandidates),
//! and realizes it once through the injected realizer.

use std::collections::BTreeMap;

use asap_cost::analytical::AnalyticalCostError;
use asap_cost::execution_demand::evaluation_rate;
use asap_cost::{materialized_tumbling_window_chains, node_map, plan_cost, CostInputs, CostModel};
use asap_types::ir::properties::{AccuracyGuarantee, BoundExpr, ProbabilityExpr};
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, QueryRoot};
use asap_types::physical::{ExecutionTiming, Lifetime, StorageMedium};
use asap_types::stages::{
    output_references, AssignmentRealizer, DagOfSelectedCandidates, ExecutionContext,
    ExecutionContextId, PhysicalASAPMemoCandidates, PhysicalCandidateError, PhysicalCandidateId,
    PruneReason, PrunedCandidate, RealizedPlan,
};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::execution_cost::{execution_costs, ExecutionCosts};
use crate::milp::error_bounds::{
    returns_its_output_unchanged, root_accuracy_limits, values_per_result_per_query,
    CandidateErrorModel, ErrorBounds, METRICS, METRIC_COUNT,
};
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
/// Accuracy: constraint (K) checks each query's composed error at its root
/// (concepts doc, section 4), unless `accuracy_checks.model` accepts the
/// query's output regardless of its guarantee. After the solve, each
/// query's composed guarantee in the selected plan is checked again with
/// the model and the deployment's evidence ([`accepted_accuracy`]).
///
/// Err: invalid or unsupported cost inputs, an infeasible catalog, a
/// solver failure, the realizer rejected the selected plan
/// ([`ErrorSelectingPlan::RealizerRejected`]: a Stage 2 catalog bug), or
/// the selected plan fails the accuracy check
/// ([`ErrorSelectingPlan::SelectedPlanFailedCheck`]).
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
    if entries.len() != accuracy_checks.query_roots.len() {
        return Err(ErrorSelectingPlan::EntryCount {
            candidate: "the accuracy check's query roots".into(),
            roots: accuracy_checks.query_roots.len(),
            entries: entries.len(),
        });
    }
    let formula_scale = cost_inputs.frozen_calibration_factor.unwrap_or(1.0);
    let contexts = ExecutionContexts::new(physical, entries);
    let costs = execution_costs(physical, &contexts, data, cost_inputs, formula_scale);
    let pruned_candidates = uncostable_candidates(physical, &contexts, &costs);
    let errors = CandidateErrorModel::new(physical, data, contexts.executions().len());
    // G_q, read by (K) and by the check after the solve.
    let values_per_result = values_per_result_per_query(accuracy_checks.query_roots, data);
    let root_limits = root_accuracy_limits(
        entries,
        &values_per_result,
        accuracy_checks.model,
        accuracy_checks.evidence,
    );
    let solved = solve_selection(&SelectionInputs::new(
        physical,
        &contexts,
        &costs,
        &errors,
        &root_limits,
    ))
    .map_err(|error| match error {
        SelectionMilpError::Infeasible => ErrorSelectingPlan::NoValidCandidate(vec![infeasible(
            &costs,
            root_limits.iter().any(Option::is_some),
        )]),
        SelectionMilpError::Solver(reason) => ErrorSelectingPlan::Solver(reason),
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
    let accepted_accuracy_per_query = accepted_accuracy(
        physical,
        &errors,
        &solved.selected_dag,
        &realized,
        entries,
        &values_per_result,
        accuracy_checks,
    )?;
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

/// The rejection of an infeasible selection: no plan meets (A)–(I), (R)
/// and, when `accuracy_checked` (it applies to some query), (K), with the
/// reasons `asap-cost` could not cost the executions the model left out,
/// each once in execution order: those executions may be why no plan is
/// left.
fn infeasible(costs: &ExecutionCosts, accuracy_checked: bool) -> Rejection {
    let mut reasons: Vec<&str> = Vec::new();
    for reason in costs.uncostable.values() {
        if !reasons.contains(&reason.as_str()) {
            reasons.push(reason);
        }
    }
    let mut reason = format!(
        "no plan gives every query output a candidate whose every input has an admissible \
         producer{} (the selection MILP is infeasible)",
        if accuracy_checked {
            " and meets its query's accuracy target at the root, constraint (K)"
        } else {
            ""
        }
    );
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

/// Per query, the accuracy accepted for the selected plan: its composed
/// guarantee in `selected` ([`composed_guarantees`]), stated over the
/// values its target's scope covers (`values_per_result[q]`, as (K)),
/// checked with the deployment's model (`AccuracyModel::satisfies`), then
/// its evidence for the realized output
/// (`AccuracyEvidenceProvider::output_guarantee`). An output the model
/// accepts regardless of its guarantee is accepted, recording the check's
/// result. Err ([`ErrorSelectingPlan::SelectedPlanFailedCheck`]) for an
/// output none accepts: a bug in the error model of (K), a model stricter
/// than the built-in rule (K) encodes, or, when (K) did not apply (a model
/// or evidence that may accept outputs regardless), an output they did not
/// accept.
fn accepted_accuracy(
    physical: &PhysicalASAPMemoCandidates,
    errors: &CandidateErrorModel,
    selected: &DagOfSelectedCandidates,
    realized: &RealizedPlan,
    entries: &[QueryWorkloadEntry],
    values_per_result: &[Option<u64>],
    accuracy_checks: &AccuracyCheckInputs<'_>,
) -> Result<Vec<AcceptedAccuracy>, ErrorSelectingPlan> {
    let composed = errors.composed_errors(selected);
    let references = output_references(&physical.query_roots);
    let model = accuracy_checks.model;
    let mut accepted = Vec::with_capacity(entries.len());
    for (query, (root, entry)) in realized.plan.roots.iter().zip(entries).enumerate() {
        let target = &entry.requirements.accuracy;
        let sketches = sketches_read(root);
        let outputs: Vec<&ErrorBounds> = references
            .iter()
            .zip(&selected.node_of_output_reference)
            .filter(|((reference_query, _), _)| *reference_query == query)
            .map(|(_, node)| &composed[node.0])
            .collect();
        let values = values_per_result[query];
        let guarantees = composed_guarantees(
            &outputs,
            returns_its_output_unchanged(&physical.query_roots[query]),
        )
        .map(|guarantees| {
            guarantees
                .into_iter()
                .map(|g| g.over_scope(target.accuracy_scope, values))
                .collect::<Vec<_>>()
        });
        let met_metric = match &guarantees {
            Some(guarantees) if guarantees.iter().all(|g| model.satisfies(g, target)) => Some(
                guarantees
                    .iter()
                    .find(|g| !g.is_exact())
                    .map(|g| g.spec.accuracy_metric),
            ),
            _ => accuracy_checks
                .evidence
                .output_guarantee(root)
                .map(|g| g.over_scope(target.accuracy_scope, values))
                .filter(|g| model.satisfies(g, target))
                .map(|g| (!g.is_exact()).then_some(g.spec.accuracy_metric)),
        };
        let rejection = match (&met_metric, &guarantees) {
            (Some(_), _) => None,
            (None, None) => Some(RejectionCause::NoAccuracyGuarantee),
            (None, Some(_)) => Some(RejectionCause::AccuracyTargetMissed),
        };
        if model.accepts_output_regardless_of_guarantee(root, target) {
            accepted.push(AcceptedAccuracy {
                target_metric: target.accuracy_metric,
                accepted_metric: None,
                accepted_regardless_of_guarantee: true,
                guarantee_check_rejection: rejection,
                sketches,
            });
            continue;
        }
        let Some(accepted_metric) = met_metric else {
            let cause = rejection.expect("not met");
            return Err(ErrorSelectingPlan::SelectedPlanFailedCheck {
                cause,
                reason: match guarantees {
                    None => format!(
                        "q{query}: the selected plan's output has no guarantee, and the \
                         deployment has no evidence that it meets target {target:?}"
                    ),
                    Some(guarantees) => format!(
                        "q{query}: the selected plan's output guarantees {:?}, which the \
                         deployment's accuracy model finds misses target {target:?}",
                        guarantees
                            .iter()
                            .map(|g| (
                                g.spec.accuracy_metric,
                                g.spec.error.evaluate(),
                                g.spec.failure_probability.evaluate()
                            ))
                            .collect::<Vec<_>>()
                    ),
                },
            });
        };
        accepted.push(AcceptedAccuracy {
            target_metric: target.accuracy_metric,
            accepted_metric,
            accepted_regardless_of_guarantee: false,
            guarantee_check_rejection: None,
            sketches,
        });
    }
    Ok(accepted)
}

/// The per-value guarantees of one query's output from the composed
/// errors of its output references' nodes: one per metric with an error
/// (the exact guarantee when there is none); `None` when part of it has no
/// guarantee, or when `unchanged` is false (a scalar root combining
/// outputs, which has no error contract) and an output is not exact.
fn composed_guarantees(
    outputs: &[&ErrorBounds],
    unchanged: bool,
) -> Option<Vec<AccuracyGuarantee>> {
    let exact = |output: &ErrorBounds| {
        !output.without_guarantee && output.error_bound.iter().all(|&e| e == 0.0)
    };
    if outputs.iter().all(|output| exact(output)) {
        return Some(vec![AccuracyGuarantee::exact("no approximation")]);
    }
    let [output] = outputs else {
        return None;
    };
    if output.without_guarantee || !unchanged {
        return None;
    }
    let failure_probability = match output.failure_probability {
        Some(0.0) => ProbabilityExpr::Zero,
        Some(delta) => ProbabilityExpr::Constant { value: delta },
        None => ProbabilityExpr::Unknown {
            statistic: "failure probability".into(),
        },
    };
    Some(
        (0..METRIC_COUNT)
            .filter(|&m| output.error_bound[m] > 0.0)
            .map(|m| {
                AccuracyGuarantee::new(
                    METRICS[m],
                    BoundExpr::Constant {
                        value: output.error_bound[m],
                    },
                    failure_probability.clone(),
                    Vec::new(),
                )
            })
            .collect(),
    )
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
