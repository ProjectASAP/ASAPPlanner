//! Stage 3, plan selection
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! section 3, "Stage 3: baseline selection and plan output", and sections
//! 4 and 5).
//!
//! [`select_plan`] reads Stage 2's physical candidates $\mathcal C$ and the
//! admissible part of the physical edges $\mathcal E$, finds every
//! execution of a candidate in an execution context $\kappa$ and every edge
//! between executions the plan may select ([`selectable`]), costs them with
//! `asap-cost` ([`cost`]), and selects with a MILP ([`milp`]) over
//! $x_{\kappa,v}$ per execution (an `AcrossEvaluations` candidate runs
//! once, $x^{\mathrm{mat}}_v$), $y_{\kappa,u,c,i}$, $z_{q,v}$, $t$, $r_s$ and
//! the retention $h_v$, under constraints (A)–(F) per context, the
//! one-consumer rule, (R) ((G) and (H) hold because $y$ exists only on
//! admissible edges whose producer delivers on the consumer's context) and
//! (X): a query reads the groups Pass 2 lists as exact-only for it through
//! exact candidates only, $a_{q,e}$ marking what its selected path reads. It
//! builds the plan (`PhysicalASAPDAG`) from the selected executions and
//! edges as its last step ([`plan`], each node retaining its candidate's
//! $h_v$, the needs from [`retention`]). Accuracy at each query root is
//! constraint (K) (`accuracy-models.md`, "Checking each query's target in
//! selection"): error bounds $E_{\kappa,c,m}$ per execution
//! and metric and failure probabilities $D_{\kappa,c}$, composed along the
//! selected edges and checked at each query's root. Costs come from
//! `asap-cost`.

mod cost;
mod milp;
mod plan;
mod report;
mod retention;
mod selectable;

use asap_cost::{
    request_overhead_cost_per_second, ClusterLimits, ClusterObjective, CostInputs, CostModel,
    LatencyCalibration, ResourceUnitCost,
};
use asap_types::deployment_capabilities::FinalResultsCache;
use asap_types::plan::physical_asap_dag::PhysicalASAPDAG;
use asap_types::stages::{MemoGroupId, PhysicalASAPMemoCandidates, PruneReason, PrunedCandidate};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::cost::{execution_costs, execution_quantities, Quantity};
use crate::milp::model::cluster::{
    solve_cluster_selection, ClusterInputs, ClusterSelectionError, FeasiblePlan,
};
use crate::milp::model::{solve_selection, SelectionInputs, SelectionMilpError};
use crate::plan::plan_of;
use crate::report::{accepted_accuracy, excluded_by_deployment, infeasible};
use crate::selectable::{Execution, QueryId, SelectableExecutions};

pub use milp::model::cluster::{ClusterLimit, ClusterLimitShortfall};
pub use report::{
    large_peak_memory_warning, AcceptedAccuracy, ErrorSelectingPlan, OptimalityGap, RejectionCause,
    Selection,
};

/// What the cluster offers of `limit`: its memory, disk or cores, the
/// precompute load bound, or the query's `max_latency_ms`.
fn available(
    limits: &ClusterLimits,
    cost_inputs: &CostInputs,
    entries: &[QueryWorkloadEntry],
    limit: ClusterLimit,
) -> f64 {
    match limit {
        ClusterLimit::MemoryBytes => limits.memory_bytes as f64,
        ClusterLimit::DiskBytes => limits.disk_bytes as f64,
        ClusterLimit::CpuCores => limits.cpu_cores.map_or(f64::INFINITY, f64::from),
        ClusterLimit::PrecomputeLoad => cost_inputs.data_plane_execution.max_thread_utilization,
        ClusterLimit::QueryLatencyMs { query } => entries
            .get(query)
            .and_then(|entry| entry.requirements.max_latency_ms)
            .unwrap_or(f64::INFINITY),
    }
}

/// Stage 3, plan selection: select one plan among `physical`'s candidates
/// (module documentation) under `cost_inputs`, and build it: one
/// `PhysicalASAPDAG` node per selected execution, its inputs from the
/// selected edges, each node's `selected_physical_candidate` set and each
/// materialized view node's `view_coverage` from its memo group
/// (`asap_types::plan::view_coverage`).
/// `entries[q]` is query `q`'s workload entry; `data` the data workload
/// with its bounds. `final_results_cache` is whether the deployment's data
/// plane caches final query results (`DeploymentCapabilities::final_results_cache`):
/// when it does, a range evaluation repeating on its step grid retains its
/// inputs for its refresh interval beyond their look-back, not for its
/// whole span ([`QueryWorkloadEntry::retained_range_span_ms`]).
///
/// Err: invalid or unsupported cost inputs, an infeasible catalog (reported
/// as excluded by the deployment when Stage 2 pruned some candidate as
/// such), or a solver failure.
pub fn select_plan(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    cost_inputs: &CostInputs,
    final_results_cache: FinalResultsCache,
) -> Result<(PhysicalASAPDAG, Selection), ErrorSelectingPlan> {
    cost_inputs
        .validate()
        .map_err(ErrorSelectingPlan::InvalidSettings)?;
    // Cost model 3's `lowest_mean_latency` selects over cost model 1's
    // structure (which executions can be costed, retention), costed at the
    // illustrative prices, with its own rows and objective.
    let cluster_limits = match &cost_inputs.cost_model {
        CostModel::UsageOverTime(_) => None,
        CostModel::ClusterCapacity(limits)
            if limits.objective == ClusterObjective::LowestMeanLatency =>
        {
            if cost_inputs.latency_calibration == LatencyCalibration::NotChecked {
                return Err(ErrorSelectingPlan::LatencyNotCalibrated);
            }
            Some(limits)
        }
        _ => {
            return Err(ErrorSelectingPlan::UnsupportedCostModel(
                "cost model 2 and cost model 3's lowest_latency and least_memory (the \
                 selection MILP implements cost model 1 and cost model 3's \
                 lowest_mean_latency)"
                    .into(),
            ))
        }
    };
    let structure_inputs;
    let cost_inputs_for_structure = match cluster_limits {
        Some(_) => {
            structure_inputs = CostInputs {
                cost_model: CostModel::UsageOverTime(ResourceUnitCost::illustrative()),
                ..cost_inputs.clone()
            };
            &structure_inputs
        }
        None => cost_inputs,
    };
    let queries = physical.logical_candidates.memo.queries();
    if entries.len() != queries.len() {
        return Err(ErrorSelectingPlan::EntryCount {
            candidate: "the physical memo".into(),
            roots: queries.len(),
            entries: entries.len(),
        });
    }
    let outputs: Vec<(QueryId, MemoGroupId)> = physical
        .logical_candidates
        .memo
        .output_groups()
        .into_iter()
        .map(|(query, group)| (QueryId(query), group))
        .collect();
    let mut selectable =
        SelectableExecutions::new(physical, entries, &outputs, final_results_cache);
    let request_overhead_cost_per_second = request_overhead_cost_per_second(entries, cost_inputs)
        .map_err(|error| ErrorSelectingPlan::Cost {
        candidate: "every plan (the request overhead)".into(),
        node: 0,
        error,
    })?;
    let costs = execution_costs(
        physical,
        &selectable,
        entries,
        data,
        cost_inputs_for_structure,
    );
    // A query output is answered by an execution that could be costed and
    // that its query may read (X).
    selectable.retain_answering(|query, index, execution| {
        costs.per_execution[index].is_ok()
            && !physical.logical_candidates.query_must_not_read(
                query.0,
                physical.physical_candidates[execution.candidate.0].logical_candidate_id,
            )
    });
    if let Some((query, _)) = selectable
        .answering()
        .iter()
        .find(|(_, executions)| executions.is_empty())
    {
        return Err(
            excluded_by_deployment(&physical.pruned_candidates, Some(query.0), &costs)
                .unwrap_or_else(|| infeasible(&costs)),
        );
    }
    let selection_inputs = SelectionInputs {
        physical,
        entries,
        selectable: &selectable,
        costs: &costs,
    };
    let milp_error = |error| match error {
        SelectionMilpError::Infeasible => {
            excluded_by_deployment(&physical.pruned_candidates, None, &costs)
                .unwrap_or_else(|| infeasible(&costs))
        }
        SelectionMilpError::Solver(reason) => ErrorSelectingPlan::Solver(reason),
        SelectionMilpError::TargetBelowSolverPrecision {
            query,
            failure_probability_bound,
        } => ErrorSelectingPlan::AccuracyTargetBelowSolverPrecision {
            query: query.0,
            failure_probability_bound,
        },
    };
    let (
        solved,
        mean_latency_ms,
        latency_ms_per_query,
        pruned_by_latency_bound,
        root_lp,
        optimality_gap,
    ) = match cluster_limits {
        None => (
            solve_selection(&selection_inputs).map_err(milp_error)?,
            None,
            None,
            0,
            None,
            None,
        ),
        Some(limits) => {
            let quantity = |quantity| {
                execution_quantities(
                    physical,
                    &selectable,
                    entries,
                    data,
                    cost_inputs_for_structure,
                    quantity,
                )
            };
            let (query_ms, refresh_ms, cpu, memory, disk) = (
                quantity(Quantity::QueryMsPerRun),
                quantity(Quantity::RefreshWallMsPerWindow),
                quantity(Quantity::CpuCoreSecondsPerSecond),
                quantity(Quantity::PeakMemoryBytes),
                quantity(Quantity::DiskBytes),
            );
            // A feasible plan's executions for T8: cost model 1's
            // optimum (when it is solved), by position.
            let feasible_plan = solve_selection(&selection_inputs).ok().map(|solved| {
                let executions = selectable.executions();
                let position =
                    |execution: &Execution| executions.iter().position(|e| e == execution);
                FeasiblePlan {
                    executions: solved.executions.iter().filter_map(position).collect(),
                    edges: selectable
                        .edges()
                        .iter()
                        .enumerate()
                        .filter(|(_, edge)| {
                            solved.edges.iter().any(|selected| {
                                selected.edge == edge.edge
                                    && selected.consumer == executions[edge.consumer]
                                    && selected.producer == executions[edge.producer]
                            })
                        })
                        .map(|(position, _)| position)
                        .collect(),
                    answering: selectable
                        .answering()
                        .iter()
                        .zip(&solved.outputs)
                        .filter_map(|((query, _), output)| {
                            position(output).map(|index| (*query, index))
                        })
                        .collect(),
                }
            });
            let solved = solve_cluster_selection(&ClusterInputs {
                selection: selection_inputs,
                query_ms: &query_ms,
                refresh_ms: &refresh_ms,
                cpu: &cpu,
                memory: &memory,
                disk: &disk,
                limits,
                cost_inputs,
                data,
                feasible_plan,
            })
            .map_err(|error| match error {
                ClusterSelectionError::Milp(error) => milp_error(error),
                ClusterSelectionError::OverLimits(shortfalls) => {
                    ErrorSelectingPlan::OverClusterLimits(
                        shortfalls
                            .into_iter()
                            .map(|shortfall| ClusterLimitShortfall {
                                available: available(limits, cost_inputs, entries, shortfall.limit),
                                ..shortfall
                            })
                            .collect(),
                    )
                }
            })?;
            let mut per_query = vec![0.0; entries.len()];
            for &(query, latency) in &solved.latency_ms_per_query {
                if let Some(slot) = per_query.get_mut(query) {
                    *slot = latency;
                }
            }
            (
                solved.solved,
                Some(solved.mean_latency_ms),
                Some(per_query),
                solved.pruned_by_latency_bound,
                solved.root_lp_mean_latency_ms,
                Some(OptimalityGap {
                    mean_latency_ms: solved.mean_latency_ms,
                    mean_latency_lower_bound_ms: solved.mean_latency_lower_bound_ms,
                    relative_gap: ((solved.mean_latency_ms - solved.mean_latency_lower_bound_ms)
                        / solved.mean_latency_ms.max(f64::MIN_POSITIVE))
                    .max(0.0),
                }),
            )
        }
    };
    let plan = plan_of(physical, &solved, &outputs).map_err(|error| {
        ErrorSelectingPlan::Solver(format!("the selected plan is invalid: {error}"))
    })?;
    // A candidate none of whose executions could be costed.
    let mut costed = vec![None; physical.physical_candidates.len()];
    for (execution, cost) in selectable.executions().iter().zip(&costs.per_execution) {
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
        selected_plan_cost_per_second: cluster_limits.is_none().then_some(
            solved.cost_per_second_without_request_overhead + request_overhead_cost_per_second,
        ),
        milp_variable_count: solved.variable_count,
        milp_constraint_count: solved.constraint_count,
        milp_construction_ms: solved.construction_ms,
        milp_solve_ms: solved.solve_ms,
        milp_solve_count: solved.solve_count,
        selected_plan_mean_latency_ms: mean_latency_ms,
        selected_plan_latency_ms_per_query: latency_ms_per_query,
        milp_root_lp_mean_latency_ms: root_lp,
        optimality_gap,
        executions_pruned_by_latency_bound: pruned_by_latency_bound,
    };
    Ok((plan, selection))
}
