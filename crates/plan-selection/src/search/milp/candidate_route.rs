//! Driver integration for the explicit, bounded candidate-connection search.
use crate::{CostModel, PlanningModels, SelectionError, Stage1ToStage3Search};
use asap_logical_asap_optimization::memo::LogicalMemo;
use asap_logical_asap_optimization::per_computation_candidates::PerComputationCandidates;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};
use std::rc::Rc;

#[derive(Default)]
pub(crate) struct CandidateArchive {
    pub logical: Vec<crate::search::pipeline::LogicalCandidate>,
    pub physical: Vec<asap_physical_asap_optimization::PhysicalCandidate>,
    pub pruned: Vec<crate::search::pipeline::PrunedCandidate>,
}

// The driver reports price search and lexicographic/Pareto search uniformly,
// while only price search supplies a scalar objective bound.
struct RouteOutcome {
    independent_components: usize,
    catalog_exhausted: Option<bool>,
    evaluation_complete: Option<bool>,
    status: super::candidate_search::CandidateSearchStatus,
    refinements: usize,
    metrics: super::candidate_connections::CandidateSolveMetrics,
    validation_seconds: f64,
    query_cuts: usize,
    rejections: super::candidate_planner::CandidateRejectionSummary,
    selected: Option<super::candidate_connections::SelectedCandidateGraph>,
    best: Option<(
        crate::Selection,
        asap_physical_asap_optimization::PhysicalCandidate,
    )>,
    lower_bound: Option<f64>,
    relative_gap: Option<f64>,
    calibration: crate::CalibrationFit,
    calibration_scope: &'static str,
}

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
    archive: &mut CandidateArchive,
) -> crate::search::pipeline::PlanSearchResult {
    use super::candidate_calibration::CandidateCalibration;
    use crate::search::milp::candidate_configurations::enumerate_candidate_configurations;
    use crate::search::milp::candidate_inventory::build_candidate_inventory;
    use crate::search::milp::candidate_planner::CandidatePlanningContext;
    use crate::search::milp::candidate_search::{CandidateSearchLimits, CandidateSearchStatus};
    use asap_logical_asap_optimization::memo::candidates::try_populate_pass1;
    use asap_logical_asap_optimization::memo::state_groups::expand_concrete_summary_state_groups;
    use std::time::{Duration, Instant};
    let unsupported = |reason: &str| {
        SelectionError::Solver(format!(
        "candidate-connections: {reason}; no validated plan was returned for the generated finite candidate catalog."
    ))
    };
    let planning_started = Instant::now();
    let total_budget = Duration::from_millis(search.milp_time_limit_ms.unwrap_or(30_000));
    let generation_ms = local_candidate_generation_ms.unwrap_or(0.0);
    if !generation_ms.is_finite() || generation_ms < 0.0 {
        return Err(unsupported("invalid candidate generation duration"));
    }
    let generation_elapsed = Duration::try_from_secs_f64(generation_ms / 1000.0)
        .map_err(|_| unsupported("candidate generation duration exceeds supported range"))?;
    let remaining = || {
        total_budget
            .saturating_sub(generation_elapsed)
            .saturating_sub(planning_started.elapsed())
    };
    let check_budget = |phase: &str| {
        if remaining().is_zero() {
            Err(unsupported(&format!(
                "planning budget exhausted after {phase}; no unvalidated plan is returned"
            )))
        } else {
            Ok(())
        }
    };
    check_budget("local candidate generation")?;
    crate::PlanSelector::new(entries, data, models, false)?;
    let calibration_policy =
        CandidateCalibration::for_models(models).map_err(|error| unsupported(&error))?;
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
    let pruned = crate::search::query_pruning::prune_query_candidates(
        inventory,
        entries,
        data,
        models,
        if search.exhaustive { 0 } else { 64 },
    )
    .map_err(|error| unsupported(&error))?;
    let early_query_pruning_ms = started.elapsed().as_secs_f64() * 1000.0;
    check_budget("early query pruning")?;
    let pruning = pruned.report;
    let early_query_pruning_counts = [
        ("choices_checked".into(), pruning.choices_checked),
        ("choices_removed".into(), pruning.removed.len()),
        ("queries_skipped".into(), pruning.queries_skipped),
        (
            "final_choices_retained".into(),
            pruning.final_choices_retained,
        ),
        (
            "shared_producer_choices_retained".into(),
            pruning.shared_producer_choices_retained,
        ),
        (
            "non_summary_choices_retained".into(),
            pruning.non_summary_choices_retained,
        ),
    ]
    .into_iter()
    .collect();
    let early_query_pruning_examples = pruning
        .removed
        .iter()
        .take(8)
        .map(|choice| {
            format!(
                "target {} choice {}: {:?}: {}",
                choice.target,
                choice.choice,
                choice.cause,
                choice.reason.chars().take(512).collect::<String>()
            )
        })
        .collect();
    let started = Instant::now();
    let mut memo = try_populate_pass1(initial, &pruned.inventory)
        .map_err(|error| unsupported(&error.to_string()))?;
    asap_logical_asap_optimization::memo::window_candidates::add_local_window_composition(
        &mut memo, entries,
    );
    let memo = expand_concrete_summary_state_groups(memo)
        .map_err(|error| unsupported(&error.to_string()))?
        .memo;
    let memo_expansion_ms = started.elapsed().as_secs_f64() * 1000.0;
    check_budget("memo rule expansion")?;
    let groups = memo.groups.len();
    let logical_candidates = memo.candidates.iter().map(Vec::len).sum();
    let started = Instant::now();
    let source = build_candidate_inventory(&memo, &output_groups, |_| Ok(0.0))
        .map_err(|error| unsupported(&error.to_string()))?;
    let immediate_candidates = source.graph.candidates.len();
    let mut configured = enumerate_candidate_configurations(
        &source,
        &output_queries,
        entries,
        data,
        models.deployment_capabilities,
    )
    .map_err(|error| unsupported(&error.to_string()))?;
    let structural_pruning =
        super::candidate_pruning::prune(&mut configured).map_err(|error| unsupported(&error))?;
    check_budget("physical candidate generation")?;
    let context = CandidatePlanningContext {
        original_roots: &roots,
        entries,
        data,
        models,
        calibration: match calibration_policy {
            CandidateCalibration::Frozen(fit) => fit,
            CandidateCalibration::CompleteCatalog => crate::CalibrationFit::default(),
        },
    };
    super::candidate_lower_bounds::apply(&mut configured, &context)
        .map_err(|error| unsupported(&error))?;
    let inventory_construction_ms = started.elapsed().as_secs_f64() * 1000.0;
    check_budget("cost bound construction")?;
    let started = Instant::now();
    let limits = CandidateSearchLimits {
        max_refinements: 128,
        wall_time: remaining(),
    };
    let result = if matches!(
        models.plan_selection.cost_model,
        CostModel::ClusterCapacity(_)
    ) || matches!(calibration_policy, CandidateCalibration::CompleteCatalog)
        || search.exhaustive
        || search.keep_every_candidate
    {
        let result = super::candidate_objectives::optimize_objective_candidates_with_options(
            &configured,
            &context,
            limits,
            calibration_policy,
            super::candidate_objectives::ObjectiveSearchOptions {
                exhaustive: search.exhaustive,
                keep_every_candidate: search.keep_every_candidate,
            },
        )
        .map_err(|error| unsupported(&error))?;
        for (selected, physical) in result.evaluated_plans {
            let logical = selected_logical_roots(&configured, &selected, &roots)
                .map_err(|error| unsupported(&error))?;
            archive
                .logical
                .push(crate::search::pipeline::LogicalCandidate {
                    id: physical.from_logical.clone(),
                    choice: Vec::new(),
                    roots: Ok(logical),
                    pass2: None,
                });
            archive.physical.push(physical);
        }
        for rejected in result.stage2_rejected_assignments {
            let logical = selected_logical_roots(&configured, &rejected.selected_graph, &roots)
                .map_err(|error| unsupported(&error))?;
            archive
                .logical
                .push(crate::search::pipeline::LogicalCandidate {
                    id: rejected.from_logical.clone(),
                    choice: Vec::new(),
                    roots: Ok(logical),
                    pass2: None,
                });
            archive
                .pruned
                .push(crate::search::pipeline::PrunedCandidate {
                    from_logical: rejected.from_logical,
                    reason: rejected.reason,
                });
        }
        archive.logical.sort_by_key(|candidate| {
            candidate
                .id
                .strip_prefix("joint-logical-")
                .and_then(|number| number.parse::<usize>().ok())
                .unwrap_or(usize::MAX)
        });
        if result.status == CandidateSearchStatus::Infeasible
            && result.best.is_none()
            && !result.evaluation_rejections.is_empty()
        {
            return Err(SelectionError::NoValidCandidate(
                result.evaluation_rejections,
            ));
        }
        RouteOutcome {
            independent_components: 1,
            catalog_exhausted: Some(result.catalog_exhausted),
            evaluation_complete: Some(result.evaluation_complete),
            status: result.status,
            refinements: result.refinements,
            metrics: result.solve_metrics,
            validation_seconds: result.validation_seconds,
            query_cuts: result.certified_query_cuts,
            rejections: result.rejections,
            selected: result.selected_graph,
            best: result.best,
            lower_bound: None,
            relative_gap: None,
            calibration: result.calibration,
            calibration_scope: result.calibration_scope,
        }
    } else {
        let result = super::candidate_components::optimize_independent_components(
            &configured,
            &context,
            limits,
        )
        .map_err(|error| unsupported(&error))?;
        RouteOutcome {
            independent_components: result.independent_components,
            catalog_exhausted: None,
            evaluation_complete: None,
            status: result.search.status,
            refinements: result.search.refinements,
            metrics: result.search.solve_metrics,
            validation_seconds: result.search.validation_seconds,
            query_cuts: result.search.certified_query_cuts,
            rejections: result.rejections,
            selected: result.search.incumbent.map(|incumbent| incumbent.selected),
            best: result.best,
            lower_bound: Some(result.search.lower_bound),
            relative_gap: result.search.relative_gap,
            calibration: context.calibration,
            calibration_scope: if models.plan_selection.frozen_calibration_factor.is_some() {
                "provided_frozen"
            } else {
                "not_required"
            },
        }
    };
    let search_and_validation_ms = started.elapsed().as_secs_f64() * 1000.0;
    let status = match &result.status {
        CandidateSearchStatus::Optimal => "optimal".into(),
        CandidateSearchStatus::Infeasible => "infeasible_under_configured_model".into(),
        CandidateSearchStatus::RefinementLimit => "refinement_limit".into(),
        CandidateSearchStatus::TimeLimit => "time_limit".into(),
        CandidateSearchStatus::SolverStopped(reason) => format!("solver_stopped: {reason}"),
        CandidateSearchStatus::EvaluatorStopped(reason) => format!("evaluator_stopped: {reason}"),
        CandidateSearchStatus::NumericalStall => "numerical_stall".into(),
    };
    let report = crate::CandidateConnectionReport {
        independent_components: result.independent_components,
        catalog_exhausted: result.catalog_exhausted,
        evaluation_complete: result.evaluation_complete,
        catalog_scope: "Pass 1, local panes, shared summary/pane states, exact and sketch rollups, compatible TopK ranking reuse, and scan widening; finite capacity, pane-width, horizon, materialization and execution-scope configurations".into(),
        status, groups, logical_candidates, immediate_candidates,
        physical_configurations: configured.configurations.len(),
        compatible_connections: configured.inventory.graph.candidates.iter().flat_map(|candidate| &candidate.producer_requirements).map(|input| input.compatible_producers.len()).sum(),
        output_references: output_groups.len(), refinements: result.refinements,
        max_refinements: 128, time_limit_ms: search.milp_time_limit_ms.unwrap_or(30_000),
        lower_bound: result.lower_bound, relative_gap: result.relative_gap,
        objective: match &models.plan_selection.cost_model {
            CostModel::ClusterCapacity(settings) => format!("{:?}", settings.objective),
            _ => "monetary_cost".into(),
        },
        calibration_scope: result.calibration_scope.into(), calibration_factor: result.calibration.factor(),
        memo_expansion_ms, local_candidate_generation_ms, inventory_construction_ms, search_and_validation_ms,
        model_construction_ms: result.metrics.model_construction_seconds * 1000.0,
        solver_ms: result.metrics.solver_seconds * 1000.0,
        validation_ms: result.validation_seconds * 1000.0,
        solve_calls: result.metrics.solve_calls,
        peak_milp_variables: result.metrics.peak_variables,
        peak_milp_constraints: result.metrics.peak_constraints,
        validation_rejection_counts: result.rejections.counts,
        certified_query_cuts: result.query_cuts,
        early_query_pruning_ms, early_query_pruning_counts, early_query_pruning_examples,
        structurally_removed_candidates: structural_pruning.removed_candidates,
        structurally_removed_connections: structural_pruning.removed_connections,
        validation_rejection_examples: result.rejections.examples,
    };
    let Some((mut selection, physical)) = result.best else {
        let metrics =
            serde_json::to_string(&report).map_err(|error| unsupported(&error.to_string()))?;
        let reason = match result.status {
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
    let selected = result
        .selected
        .as_ref()
        .ok_or_else(|| unsupported("validated plan has no selected graph"))?;
    let logical = selected_logical_roots(&configured, selected, &roots)
        .map_err(|error| unsupported(&error))?;
    selection.search.candidates_checked = result.refinements as u64;
    selection.search.candidate_connections = Some(report);
    Ok((selection, physical, logical))
}

fn selected_logical_roots(
    configured: &super::candidate_configurations::CandidateConfigurationInventory,
    selected: &super::candidate_connections::SelectedCandidateGraph,
    roots: &[QueryRoot],
) -> Result<Vec<QueryRoot>, String> {
    let outputs = super::candidate_reconstruction::reconstruct_selected_candidates(
        &configured.inventory.graph,
        selected,
        &configured.inventory.templates,
    )?
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
    Ok(logical)
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

    // Earlier candidate generation consumes the same budget as solver work.
    #[test]
    fn generation_time_is_charged_before_search_starts() {
        let (roots, entries, data) = workload();
        let initial = LogicalMemo::from_roots(&roots);
        let inventory = asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates(roots).unwrap();
        let error = candidate_connection_search(
            initial,
            &inventory,
            &entries,
            &data,
            PlanningModels::builtin(),
            Stage1ToStage3Search {
                search: SearchAlgorithm::CandidateConnections,
                milp_time_limit_ms: Some(1),
                ..Default::default()
            },
            Some(2.0),
            &mut CandidateArchive::default(),
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("planning budget exhausted after local candidate generation"));
    }

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
        assert!(report.catalog_scope.contains("finite capacity"));
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
        asap_logical_asap_optimization::memo::window_candidates::add_local_window_composition(
            &mut memo, &entries,
        );
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

    // Exhaustive diagnostics retain logical and physical plans on this route.
    #[test]
    fn candidate_connections_exports_exhaustive_archive() {
        let (_, entries, data) = workload();
        let roots = vec![(
            0,
            QueryRoot::Scalar(asap_types::ir::ScalarExpr::Literal(
                asap_types::ir::scalar::ScalarValue::Float64(1.0),
            )),
        )];
        let run = run_stage1_to_stage3_with(
            roots,
            &entries[..1],
            &data,
            PlanningModels::builtin(),
            Stage1ToStage3Search {
                exhaustive: true,
                keep_every_candidate: true,
                ..Stage1ToStage3Search::with_algorithm(SearchAlgorithm::CandidateConnections)
            },
        )
        .unwrap();
        let selection = run.stage3_plan_selection.unwrap();
        assert_eq!(
            selection.search.algorithm,
            SearchAlgorithm::CandidateConnections
        );
        assert_eq!(run.stage1_logical_asap_optimization.len(), 1);
        assert_eq!(run.stage2_physical_asap_optimization.len(), 1);
        assert_eq!(
            run.stage2_physical_asap_optimization[0].from_logical,
            run.stage1_logical_asap_optimization[0].id
        );
        assert_eq!(
            selection.search.candidate_connections.unwrap().status,
            "optimal"
        );
    }
}
