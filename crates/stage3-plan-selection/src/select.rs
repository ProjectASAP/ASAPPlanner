//! Stage 3, plan selection: [`select_plan`] finds every execution of a
//! candidate in a context and every edge between executions the plan may
//! select ([`crate::milp::execution_contexts`]), costs them
//! ([`crate::cost`]), builds and solves the selection MILP over them
//! ([`crate::milp::model`]), and builds the plan from the selected
//! executions and edges as its last step ([`crate::plan`]).

use std::collections::BTreeSet;

use asap_accuracy::readout_metric;
use asap_cost::{summary_type_name, CostInputs, CostModel};
use asap_types::ir::operator::asap::summary_type_of_operator;
use asap_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPNodeId};
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, Operator, QueryRoot};
use asap_types::stages::{
    output_references, MemoGroupId, PhysicalASAPMemoCandidates, PruneReason, PrunedCandidate,
};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::cost::{execution_costs, ExecutionCosts};
use crate::milp::execution_contexts::{Execution, ExecutionContexts};
use crate::milp::model::{solve_selection, QueryId, SelectionInputs, SelectionMilpError};
use crate::plan::plan_of;
use crate::report::{AcceptedAccuracy, ErrorSelectingPlan, Rejection, RejectionCause, Selection};

/// Stage 3, plan selection: select one plan among `physical`'s candidates
/// (module documentation) under `cost_inputs`, and build it: one
/// `PhysicalASAPDAG` node per selected execution, its inputs from the
/// selected edges, each node's `selected_physical_candidate` set.
/// `entries[q]` is query `q`'s workload entry; `data` the data workload
/// with its bounds.
///
/// Err: invalid or unsupported cost inputs, an infeasible catalog, or a
/// solver failure.
pub fn select_plan(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    cost_inputs: &CostInputs,
) -> Result<(PhysicalASAPDAG, Selection), ErrorSelectingPlan> {
    cost_inputs
        .validate()
        .map_err(ErrorSelectingPlan::InvalidSettings)?;
    if !matches!(cost_inputs.cost_model, CostModel::UsageOverTime(_)) {
        return Err(ErrorSelectingPlan::UnsupportedCostModel(
            "cost models 2 and 3 (the selection MILP implements cost model 1 only)".into(),
        ));
    }
    let queries = &physical.logical_candidates.memo.queries;
    if entries.len() != queries.len() {
        return Err(ErrorSelectingPlan::EntryCount {
            candidate: "the physical memo".into(),
            roots: queries.len(),
            entries: entries.len(),
        });
    }
    let formula_scale = cost_inputs.frozen_calibration_factor.unwrap_or(1.0);
    let roots: Vec<QueryRoot<MemoGroupId>> =
        queries.iter().map(|query| query.root.clone()).collect();
    let outputs: Vec<(QueryId, MemoGroupId)> = output_references(&roots)
        .into_iter()
        .map(|(query, group)| (QueryId(query), group))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let (contexts, output_executions) = ExecutionContexts::new(physical, entries, &outputs);
    let costs = execution_costs(
        physical,
        &contexts,
        &output_executions,
        entries,
        data,
        cost_inputs,
        formula_scale,
    );
    let query_outputs: Vec<(QueryId, Vec<Execution>)> = output_executions
        .into_iter()
        .map(|(query, executions)| {
            let costed = executions
                .into_iter()
                .filter(|&execution| costs.per_execution[contexts.index_of(execution)].is_ok())
                .collect();
            (query, costed)
        })
        .collect();
    if query_outputs
        .iter()
        .any(|(_, executions)| executions.is_empty())
    {
        return Err(infeasible(&costs));
    }
    let solved = solve_selection(&SelectionInputs {
        physical,
        entries,
        contexts: &contexts,
        costs: &costs,
        query_outputs: &query_outputs,
    })
    .map_err(|error| match error {
        SelectionMilpError::Infeasible => infeasible(&costs),
        SelectionMilpError::Solver(reason) => ErrorSelectingPlan::Solver(reason),
    })?;
    let plan = plan_of(physical, &solved, &outputs).map_err(|error| {
        ErrorSelectingPlan::Solver(format!("the selected plan is invalid: {error}"))
    })?;
    // A candidate none of whose executions could be costed.
    let mut costed = vec![None; physical.physical_candidates.len()];
    for (execution, cost) in contexts.executions().iter().zip(&costs.per_execution) {
        let known = costed[execution.candidate.0].get_or_insert(false);
        *known |= cost.is_ok();
    }
    let selection = Selection {
        accepted_accuracy_per_query: accepted_accuracy(physical, &plan),
        pruned_candidates: costed
            .iter()
            .zip(&physical.physical_candidates)
            .filter(|(costed, _)| **costed == Some(false))
            .map(|(_, &candidate)| PrunedCandidate {
                candidate,
                reason: PruneReason::UsageNotCostable,
            })
            .collect(),
        milp_variables: solved.variables,
        milp_constraints: solved.constraints,
        milp_construction_ms: solved.construction_ms,
        milp_solve_ms: solved.solve_ms,
    };
    Ok((plan, selection))
}

/// The selection MILP is infeasible, with the reasons executions could not
/// be costed.
fn infeasible(costs: &ExecutionCosts) -> ErrorSelectingPlan {
    let mut reasons: Vec<&str> = Vec::new();
    for reason in costs
        .per_execution
        .iter()
        .filter_map(|cost| cost.as_ref().err())
    {
        if !reasons.contains(&reason.as_str()) {
            reasons.push(reason);
        }
    }
    let mut reason = "no plan gives every query output a candidate whose every input has an \
                      admissible producer that delivers on its context (the selection MILP is \
                      infeasible)"
        .to_string();
    if !reasons.is_empty() {
        reason += &format!(
            "; executions left out because they could not be costed: {}",
            reasons.join("; ")
        );
    }
    ErrorSelectingPlan::NoValidCandidate(vec![Rejection {
        id: "workload".into(),
        cause: RejectionCause::Infeasible,
        reason,
    }])
}

/// Per query, the sketches its output reads in `plan` (each sketch build
/// below its root, in node order) and, when it has an accuracy target and
/// reads a sketch, the metric of the first sketch readout below its root
/// ([`readout_metric`]).
fn accepted_accuracy(
    physical: &PhysicalASAPMemoCandidates,
    plan: &PhysicalASAPDAG,
) -> Vec<AcceptedAccuracy> {
    let memo = &physical.logical_candidates.memo;
    let mut operator_roots = plan.roots.iter();
    memo.queries
        .iter()
        .enumerate()
        .map(|(query, memo_query)| {
            let starts: Vec<PhysicalASAPNodeId> = match plan
                .scalar_roots
                .iter()
                .find(|(position, _)| *position == query)
            {
                Some((_, expression)) => expression.operator_refs().into_iter().copied().collect(),
                None => vec![*operator_roots.next().expect("one operator root per query")],
            };
            let below = nodes_below(plan, starts);
            let mut sketches: Vec<String> = Vec::new();
            let mut readout_metrics = Vec::new();
            for node in plan.nodes.iter().filter(|node| below.contains(&node.id)) {
                match &node.payload {
                    Operator::ASAP(
                        ASAPOp::SummaryAgg { summary_type, .. }
                        | ASAPOp::SummaryTumblingWindowChain { summary_type, .. },
                    ) if matches!(summary_type, FieldDataType::Sketch(_)) => {
                        let name = summary_type_name(summary_type);
                        if !sketches.contains(&name) {
                            sketches.push(name);
                        }
                    }
                    Operator::ASAP(ASAPOp::SummaryEstimate { query, .. }) => {
                        // The state group the readout's candidate reads.
                        let readout = node.selected_physical_candidate.and_then(|v| {
                            match memo
                                .candidate(physical.physical_candidates[v.0].logical_candidate_id)
                                .expression
                                .operator
                            {
                                Operator::ASAP(ASAPOp::SummaryEstimate {
                                    summary_input, ..
                                }) => summary_type_of_operator(
                                    &memo.groups()[summary_input.0 as usize].expression,
                                ),
                                _ => None,
                            }
                        });
                        readout_metrics.extend(
                            readout.and_then(|summary_type| readout_metric(summary_type, query)),
                        );
                    }
                    _ => {}
                }
            }
            let has_accuracy_target = memo_query.accuracy_target.is_some();
            AcceptedAccuracy {
                has_accuracy_target,
                accepted_metric: (has_accuracy_target && !sketches.is_empty())
                    .then(|| readout_metrics.first().copied())
                    .flatten(),
                sketches,
            }
        })
        .collect()
}

/// The nodes at or below `starts` in `plan`.
fn nodes_below(
    plan: &PhysicalASAPDAG,
    starts: Vec<PhysicalASAPNodeId>,
) -> BTreeSet<PhysicalASAPNodeId> {
    let mut below = BTreeSet::new();
    let mut pending = starts;
    while let Some(id) = pending.pop() {
        if below.insert(id) {
            pending.extend(
                plan.edges
                    .iter()
                    .filter(|e| e.consumer == id)
                    .map(|e| e.producer),
            );
        }
    }
    below
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use asap_types::ir::operator::operator_properties::Reduction;
    use asap_types::ir::physical_export::{
        EdgeRole, GroupingEdgeCompatibility, PhysicalASAPDAGEdge, WindowEdgeCompatibility,
    };
    use asap_types::ir::properties::{AccuracyMetric, AccuracyTarget, ExecutionDataState};
    use asap_types::ir::scalar::expr_ir::ColumnRef;
    use asap_types::ir::schema::{SketchAlgorithm, SketchStatistic, SummaryUpdate};
    use asap_types::ir::{NonASAPOp, TimeRangeKind};
    use asap_types::physical::{ExecutionTiming, Lifetime, PhysicalProperties};
    use asap_types::stages::{
        parameter_list, CandidateForMemoGroupId, LogicalASAPMemoCandidates, Memo, MemoQuery,
        PhysicalCandidate, PhysicalCandidateId,
    };
    use asap_types::test_support::{
        add_candidate, group_with_own_candidate, series_schema, time_series_scan,
    };

    use super::*;
    use crate::cost::candidate_node;

    /// `quantile_over_time(0.5, m[1m])` answered by a DDSketch readout,
    /// under `accuracy_target`: the physical memo and the plan reading it.
    fn ddsketch_plan(
        accuracy_target: Option<AccuracyTarget>,
    ) -> (PhysicalASAPMemoCandidates, PhysicalASAPDAG) {
        let mut memo = Memo::default();
        let scan =
            group_with_own_candidate(&mut memo, time_series_scan("m", series_schema(&[]), vec![]));
        let last_minute = group_with_own_candidate(
            &mut memo,
            Operator::NonASAP(NonASAPOp::TimeRange {
                range: Duration::from_secs(60),
                kind: TimeRangeKind::Range,
                child: scan,
            }),
        );
        let build = Operator::ASAP(ASAPOp::SummaryAgg {
            child: last_minute,
            summary_type: FieldDataType::Sketch(SketchAlgorithm::DDSketch),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            filter: None,
        });
        let state = memo.find_or_add_group(build.clone()).unwrap();
        let parameters = parameter_list(SketchAlgorithm::DDSketch)[0].clone();
        add_candidate(&mut memo, state, build, Some(parameters)).unwrap();
        let readout = group_with_own_candidate(
            &mut memo,
            Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input: state,
                query: SketchStatistic::Quantile { q: 0.5 },
            }),
        );
        memo.add_query(MemoQuery {
            root: QueryRoot::Operator(readout),
            accuracy_target,
            values_per_evaluation: None,
        });
        let groups = [scan, last_minute, state, readout];
        let physical = PhysicalASAPMemoCandidates::new(
            LogicalASAPMemoCandidates {
                evaluation_grid_of_group: vec![None; memo.groups().len()],
                memo,
                edges: vec![],
                pruned: vec![],
            },
            groups
                .iter()
                .map(|&group| PhysicalCandidate {
                    logical_candidate_id: CandidateForMemoGroupId {
                        group,
                        index_in_group: 0,
                    },
                    physical_properties: PhysicalProperties::new(
                        ExecutionTiming::QueryTime,
                        Lifetime::None,
                    )
                    .unwrap(),
                })
                .collect(),
            vec![],
            vec![],
        );
        // Node i is candidate i, reading node i - 1.
        let nodes = (0..groups.len())
            .map(|i| {
                let inputs: Vec<PhysicalASAPNodeId> = i.checked_sub(1).into_iter().collect();
                candidate_node(&physical, PhysicalCandidateId(i), i, &inputs)
            })
            .collect::<Vec<_>>();
        let edges = (1..groups.len())
            .map(|i| {
                PhysicalASAPDAGEdge::new(
                    i - 1,
                    i,
                    EdgeRole::Input,
                    nodes[i - 1].output_schema.clone(),
                    ExecutionDataState::QUERY_ROWS,
                    GroupingEdgeCompatibility::NotApplicable,
                    WindowEdgeCompatibility::NotApplicable,
                )
            })
            .collect();
        let plan = PhysicalASAPDAG {
            nodes,
            edges,
            roots: vec![groups.len() - 1],
            scalar_roots: vec![],
        };
        (physical, plan)
    }

    /// The accepted metric is the readout's, here DDSketch's relative
    /// error, also when the target sets no metric; without a target there
    /// is none to accept (was "accepted exact (DDSketch)" for both).
    #[test]
    fn the_accepted_metric_is_the_readout_s() {
        let target = AccuracyTarget {
            accuracy_metric: None,
            max_error: 0.01,
            failure_probability: None,
            accuracy_scope: asap_types::ir::properties::AccuracyScope::PerValue,
        };
        let (physical, plan) = ddsketch_plan(Some(target));
        let accepted = accepted_accuracy(&physical, &plan);
        assert_eq!(accepted[0].accepted_metric, Some(AccuracyMetric::Relative));
        assert_eq!(accepted[0].to_string(), "accepted relative (DDSketch)");

        let (physical, plan) = ddsketch_plan(None);
        let accepted = accepted_accuracy(&physical, &plan);
        assert_eq!(accepted[0].accepted_metric, None);
        assert_eq!(accepted[0].to_string(), "no accuracy target (DDSketch)");
    }
}
