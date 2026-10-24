//! Driver integration for the explicit, bounded candidate-connection search.
use crate::{CostModel, PlanningModels, SelectionError, Stage1ToStage3Search};
use asap_logical_asap_optimization::memo::LogicalMemo;
use asap_logical_asap_optimization::per_computation_candidates::PerComputationCandidates;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};
use std::rc::Rc;

/// Public driver integration for the explicitly named, restricted candidate
/// catalog. Existing algorithms retain their broader rule sets and objectives.
pub(crate) fn candidate_connection_search<Id: Clone>(
    initial: LogicalMemo,
    inventory: &PerComputationCandidates<Id>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    search: Stage1ToStage3Search,
    local_candidate_generation_ms: Option<f64>,
) -> crate::search::milp::model::MemoSearchResult {
    use crate::search::milp::candidate_configurations::enumerate_candidate_configurations;
    use crate::search::milp::candidate_inventory::build_candidate_inventory;
    use crate::search::milp::candidate_planner::{
        optimize_configured_candidates, CandidatePlanningContext,
    };
    use crate::search::milp::candidate_reconstruction::reconstruct_selected_candidates;
    use crate::search::milp::candidate_search::{CandidateSearchLimits, CandidateSearchStatus};
    use asap_logical_asap_optimization::memo::candidates::try_populate_pass1;
    use asap_logical_asap_optimization::memo::state_groups::expand_concrete_summary_state_groups;
    use std::time::{Duration, Instant};
    let unsupported = |reason: &str| {
        SelectionError::Solver(format!(
        "candidate-connections: {reason}; this route currently supports Pass 1, local panes, and concrete shared summary/pane states. Use memo-milp or legacy for the existing broader rule catalog."
    ))
    };
    if search.exhaustive || search.keep_every_candidate {
        return Err(unsupported(
            "exhaustive/keep-every-candidate output is not implemented",
        ));
    }
    if !matches!(
        models.plan_selection.cost_model,
        CostModel::UsageOverTime(_) | CostModel::ProvisionedCapacity(_)
    ) {
        return Err(unsupported(
            "ClusterCapacity objectives require their existing capacity/Pareto route",
        ));
    }
    let selector = crate::PlanSelector::new(entries, data, models, false)?;
    if selector.needs_calibration() {
        return Err(unsupported("measured tables need an externally frozen workload calibration fit, which this driver does not yet accept"));
    }
    let roots = inventory
        .roots
        .iter()
        .map(|(_, root)| root.clone())
        .collect::<Vec<_>>();
    let mut output_groups = Vec::new();
    let mut output_queries = Vec::new();
    let mut missing_group = false;
    for (query, root) in roots.iter().enumerate() {
        let mut reference = |node: &Rc<OperatorNode>| {
            if let Some(group) = initial.group_for_node(node) {
                output_groups.push(group);
                output_queries.push(query);
            } else {
                missing_group = true;
            }
            Rc::clone(node)
        };
        match root {
            QueryRoot::Operator(node) => {
                reference(node);
            }
            QueryRoot::Scalar(expression) => {
                expression.map_operator_refs(&mut reference);
            }
        }
    }
    if missing_group {
        return Err(unsupported(
            "an output reference is absent from the initial memo",
        ));
    }
    let started = Instant::now();
    let mut memo =
        try_populate_pass1(initial, inventory).map_err(|error| unsupported(&error.to_string()))?;
    crate::search::memo::rules::windows::add_local_window_composition(&mut memo, entries);
    let memo = expand_concrete_summary_state_groups(memo)
        .map_err(|error| unsupported(&error.to_string()))?
        .memo;
    let memo_expansion_ms = started.elapsed().as_secs_f64() * 1000.0;
    let groups = memo.groups.len();
    let logical_candidates = memo.candidates.iter().map(Vec::len).sum();
    let started = Instant::now();
    let source = build_candidate_inventory(&memo, &output_groups, |_| Ok(0.0))
        .map_err(|error| unsupported(&error.to_string()))?;
    let immediate_candidates = source.graph.candidates.len();
    let configured = enumerate_candidate_configurations(
        &source,
        &output_queries,
        entries,
        data,
        models.deployment_capabilities,
    )
    .map_err(|error| unsupported(&error.to_string()))?;
    let inventory_construction_ms = started.elapsed().as_secs_f64() * 1000.0;
    let context = CandidatePlanningContext {
        original_roots: &roots,
        entries,
        data,
        models,
        calibration: crate::CalibrationFit::default(),
    };
    let started = Instant::now();
    let result = optimize_configured_candidates(
        &configured,
        &context,
        CandidateSearchLimits {
            max_refinements: 128,
            wall_time: Duration::from_millis(search.milp_time_limit_ms.unwrap_or(30_000)),
        },
    )
    .map_err(|error| unsupported(&error))?;
    let search_and_validation_ms = started.elapsed().as_secs_f64() * 1000.0;
    let status = match &result.search.status {
        CandidateSearchStatus::Optimal => "optimal".into(),
        CandidateSearchStatus::Infeasible => "infeasible_under_configured_model".into(),
        CandidateSearchStatus::RefinementLimit => "refinement_limit".into(),
        CandidateSearchStatus::TimeLimit => "time_limit".into(),
        CandidateSearchStatus::SolverStopped(reason) => format!("solver_stopped: {reason}"),
        CandidateSearchStatus::EvaluatorStopped(reason) => format!("evaluator_stopped: {reason}"),
        CandidateSearchStatus::NumericalStall => "numerical_stall".into(),
    };
    let report = crate::CandidateConnectionReport {
        catalog_scope: "Pass 1 + local panes + identical-configuration shared summary/pane states with compatible per-entity scan-label widening; cross-width/horizon pane reuse, rollup, general shared scans and top-k-reuse expansion is not included".into(),
        status, groups, logical_candidates, immediate_candidates,
        physical_configurations: configured.configurations.len(),
        compatible_connections: configured.inventory.graph.candidates.iter().flat_map(|candidate| &candidate.producer_requirements).map(|input| input.compatible_producers.len()).sum(),
        output_references: output_groups.len(), refinements: result.search.refinements,
        max_refinements: 128, time_limit_ms: search.milp_time_limit_ms.unwrap_or(30_000),
        lower_bound: result.search.lower_bound, relative_gap: result.search.relative_gap,
        memo_expansion_ms, local_candidate_generation_ms, inventory_construction_ms, search_and_validation_ms,
        model_construction_ms: result.search.solve_metrics.model_construction_seconds * 1000.0,
        solver_ms: result.search.solve_metrics.solver_seconds * 1000.0,
        validation_ms: result.search.validation_seconds * 1000.0,
        solve_calls: result.search.solve_metrics.solve_calls,
        peak_milp_variables: result.search.solve_metrics.peak_variables,
        peak_milp_constraints: result.search.solve_metrics.peak_constraints,
        validation_rejection_counts: result.rejections.counts,
        certified_query_cuts: result.search.certified_query_cuts,
        validation_rejection_examples: result.rejections.examples,
    };
    let Some((mut selection, physical)) = result.best else {
        let metrics =
            serde_json::to_string(&report).map_err(|error| unsupported(&error.to_string()))?;
        let reason = match result.search.status {
            CandidateSearchStatus::Infeasible => {
                "generated candidate catalog is infeasible under the configured model"
            }
            CandidateSearchStatus::TimeLimit | CandidateSearchStatus::RefinementLimit => {
                "planning budget ended before a validated incumbent was found"
            }
            _ => "search stopped without a validated incumbent; inspect the termination diagnostic",
        };
        return Err(unsupported(&format!("{reason}. Search report: {metrics}")));
    };
    let selected = &result
        .search
        .incumbent
        .as_ref()
        .ok_or_else(|| unsupported("validated plan has no search incumbent"))?
        .selected;
    let outputs = reconstruct_selected_candidates(
        &configured.inventory.graph,
        selected,
        &configured.inventory.templates,
    )
    .map_err(|error| unsupported(&error))?
    .outputs;
    let mut next = outputs.iter();
    let logical = roots
        .iter()
        .map(|root| match root {
            QueryRoot::Operator(_) => {
                QueryRoot::Operator(Rc::clone(next.next().expect("validated output mapping")))
            }
            QueryRoot::Scalar(expression) => {
                QueryRoot::Scalar(expression.map_operator_refs(&mut |_| {
                    Rc::clone(next.next().expect("validated output mapping"))
                }))
            }
        })
        .collect();
    selection.search.candidates_checked = result.search.refinements as u64;
    selection.search.candidate_connections = Some(report);
    Ok((selection, physical, logical))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{run_stage1_to_stage3_with, SearchAlgorithm};
    use asap_logical_asap_optimization::test_support::{entry_with, lower_promql};
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::workload::{
        DataArrival, Predictability, QueryRecurrence, RepeatedDemand, RepetitionInterval,
        SourceFacts,
    };

    fn workload() -> (
        Vec<(usize, QueryRoot)>,
        Vec<QueryWorkloadEntry>,
        DataWorkload,
    ) {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let roots = [0.95, 0.99]
            .iter()
            .enumerate()
            .map(|(id, quantile)| {
                (
                    id,
                    QueryRoot::Operator(lower_promql(
                        &format!("quantile_over_time({quantile}, latency[5m])"),
                        target.clone(),
                    )),
                )
            })
            .collect();
        let entries = (0..2)
            .map(|_| {
                let mut entry = entry_with(
                    target.clone(),
                    QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
                        300_000,
                    ))),
                );
                entry.predictability = Predictability::Predictable { known_at: None };
                entry
            })
            .collect();
        let data = DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 1000.0,
            },
            sources: [(
                "latency".into(),
                SourceFacts {
                    series_count: Some(1),
                    ..Default::default()
                },
            )]
            .into(),
        };
        (roots, entries, data)
    }

    // The standard stage driver exposes the new route without misreporting its catalog.
    #[test]
    fn candidate_connections_driver_returns_validated_plan_and_scope_report() {
        let (roots, entries, data) = workload();
        let run = run_stage1_to_stage3_with(
            roots,
            &entries,
            &data,
            PlanningModels::builtin(),
            Stage1ToStage3Search {
                milp_time_limit_ms: Some(10_000),
                ..Stage1ToStage3Search::with_algorithm(SearchAlgorithm::CandidateConnections)
            },
        )
        .unwrap();
        let selection = run.stage3_plan_selection.unwrap();
        assert_eq!(
            selection.search.algorithm,
            SearchAlgorithm::CandidateConnections
        );
        assert!(selection.search.routed_to_legacy.is_none());
        assert!(selection.search.milp.is_none());
        let report = selection.search.candidate_connections.unwrap();
        assert!(report.catalog_scope.contains("not included"));
        assert!(report.physical_configurations >= report.immediate_candidates);
        assert!(report.refinements > 0);
        assert!(report.solve_calls > 0);
        assert!(report.peak_milp_variables > 0);
        assert!(report.peak_milp_constraints > 0);
        assert_eq!(report.output_references, 2);
        run.stage3_selected.unwrap().dag.validate().unwrap();
        assert!(run.stage1_selected.is_some());
    }

    // Pane rewriting must preserve canonical inputs so equal chains can share state.
    #[test]
    fn local_pane_rules_expose_identical_chains_to_state_groups() {
        use asap_logical_asap_optimization::memo::candidates::try_populate_pass1;
        use asap_logical_asap_optimization::memo::state_groups::expand_concrete_summary_state_groups;
        use asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates;
        let (roots, mut entries, _) = workload();
        for entry in &mut entries {
            entry.recurrence =
                QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(1_000)));
        }
        let initial = LogicalMemo::from_roots(&roots);
        let generated = generate_per_computation_candidates(roots).unwrap();
        let mut memo = try_populate_pass1(initial, &generated).unwrap();
        crate::search::memo::rules::windows::add_local_window_composition(&mut memo, &entries);
        let expanded = expand_concrete_summary_state_groups(memo).unwrap();
        assert!(
            expanded.state_groups.iter().any(|group| {
                let state = &expanded.memo.groups[group.0 as usize];
                matches!(
                    state.node.asap(),
                    Some(asap_types::ir::ASAPOp::PaneChain { .. })
                ) && state.queries == vec![0, 1]
            }),
            "identical panes must expose a common state producer"
        );
    }

    // Sliding evaluations without retained raw data need a maintained pane chain.
    #[test]
    fn candidate_connections_uses_panes_for_frequent_window_evaluations() {
        let (mut roots, mut entries, data) = workload();
        roots.truncate(1);
        entries.truncate(1);
        entries[0].recurrence =
            QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(1_000)));
        let deployment = asap_logical_asap_optimization::DeploymentCapabilities {
            raw_data_retained: false,
            query_time_results_retained: false,
            ..Default::default()
        };
        let models = PlanningModels {
            deployment_capabilities: &deployment,
            ..PlanningModels::builtin()
        };
        let run = run_stage1_to_stage3_with(
            roots,
            &entries,
            &data,
            models,
            Stage1ToStage3Search {
                milp_time_limit_ms: Some(10_000),
                ..Stage1ToStage3Search::with_algorithm(SearchAlgorithm::CandidateConnections)
            },
        )
        .unwrap();
        let selected = run.stage3_plan_selection.unwrap();
        assert!(selected
            .search
            .candidate_connections
            .as_ref()
            .unwrap()
            .local_candidate_generation_ms
            .is_some());
        let physical = run.stage3_selected.unwrap();
        assert!(physical
            .roots
            .iter()
            .flat_map(|root| match root {
                QueryRoot::Operator(node) => OperatorNode::reachable(node),
                QueryRoot::Scalar(_) => Vec::new(),
            })
            .any(|node| matches!(node.asap(), Some(asap_types::ir::ASAPOp::PaneChain { .. }))));
    }

    // Unsupported presentation/objective requests never silently route to legacy.
    #[test]
    fn candidate_connections_rejects_exhaustive_request_explicitly() {
        let (roots, entries, data) = workload();
        let run = run_stage1_to_stage3_with(
            roots,
            &entries,
            &data,
            PlanningModels::builtin(),
            Stage1ToStage3Search {
                exhaustive: true,
                ..Stage1ToStage3Search::with_algorithm(SearchAlgorithm::CandidateConnections)
            },
        )
        .unwrap();
        assert!(
            matches!(run.stage3_plan_selection, Err(SelectionError::Solver(reason)) if reason.contains("exhaustive") && reason.contains("broader rule catalog"))
        );
        assert!(run.stage3_selected.is_none());
        assert_eq!(
            serde_json::to_string(&SearchAlgorithm::CandidateConnections).unwrap(),
            "\"candidate-connections\""
        );
    }
}
