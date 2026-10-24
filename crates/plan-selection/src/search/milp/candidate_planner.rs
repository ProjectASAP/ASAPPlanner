//! Opt-in evaluator-backed search over configured candidate connections.
//!
//! This is not a production routing change. Every proposal passes real Stage 2
//! and PlanSelector; only a validated price becomes an exact refinement cut.
use asap_physical_asap_optimization::PhysicalCandidate;
use asap_types::ir::QueryRoot;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};
use std::collections::BTreeMap;

use super::candidate_configurations::CandidateConfigurationInventory;
use super::candidate_connections::SelectedCandidateGraph;
use super::candidate_physical::{materialize_selected_candidates, PhysicalCandidateError};
use super::candidate_search::{
    search_candidates_with_weighted_baseline, CandidateSearchLimits, CandidateSearchResult,
};
use crate::{CalibrationFit, CostModel, PlanSelector, PlanningModels, Selection, SelectionError};

pub(super) struct CandidatePlanningContext<'a> {
    pub original_roots: &'a [QueryRoot],
    pub entries: &'a [QueryWorkloadEntry],
    pub data: &'a DataWorkload,
    pub models: PlanningModels<'a>,
    /// One externally established fit, held constant across every proposal.
    pub calibration: CalibrationFit,
}

/// Bounded evidence explaining rejected assignments, not complete plan history.
#[derive(Debug, Default)]
pub(super) struct CandidateRejectionSummary {
    pub counts: BTreeMap<String, usize>,
    pub examples: Vec<String>,
}

impl CandidateRejectionSummary {
    fn record(&mut self, category: String, reason: &str) {
        *self.counts.entry(category.clone()).or_default() += 1;
        let example = format!(
            "{category}: {}",
            reason.chars().take(512).collect::<String>()
        );
        if self.examples.len() < 8 && !self.examples.contains(&example) {
            self.examples.push(example);
        }
    }
}

pub(super) struct CandidatePlanningResult {
    pub search: CandidateSearchResult,
    pub rejections: CandidateRejectionSummary,
    /// Same convention as PlanSelector::finish: Selection.plan is the validated
    /// execution plan; PhysicalCandidate is the offered Stage 2 candidate.
    pub best: Option<(Selection, PhysicalCandidate)>,
}

/// Usage and provisioned-price objectives use the complete evaluator cost,
/// including held memory, raw retention and request overhead where applicable.
/// ClusterCapacity is intentionally unsupported: its Pareto/lexicographic
/// objective cannot be replaced by a scalar cost silently.
pub(super) fn optimize_configured_candidates(
    configured: &CandidateConfigurationInventory,
    context: &CandidatePlanningContext<'_>,
    limits: CandidateSearchLimits,
) -> Result<CandidatePlanningResult, String> {
    if !matches!(
        context.models.plan_selection.cost_model,
        CostModel::UsageOverTime(_) | CostModel::ProvisionedCapacity(_)
    ) {
        return Err("candidate price search does not support ClusterCapacity objectives".into());
    }
    if configured.configurations.len() != configured.inventory.graph.candidates.len() {
        return Err("physical configurations must be indexed by candidate ID".into());
    }
    if context.original_roots.len() != context.entries.len() {
        return Err("original roots and workload entries differ in length".into());
    }
    // Reject invalid settings/deployment even if the search budget is zero.
    PlanSelector::new(context.entries, context.data, context.models, false)
        .map_err(|e| e.to_string())?;
    if !context.calibration.factor().is_finite() || context.calibration.factor() <= 0.0 {
        return Err("candidate search requires a finite positive frozen calibration".into());
    }
    let mut best: Option<(f64, SelectedCandidateGraph, Selection, PhysicalCandidate)> = None;
    let mut evaluation_number = 0;
    let mut rejections = CandidateRejectionSummary::default();
    let weighting_started = std::time::Instant::now();
    let weights = accuracy_baseline_weights(configured, context)?;
    let search = search_candidates_with_weighted_baseline(
        &configured.inventory.graph,
        CandidateSearchLimits {
            wall_time: limits.wall_time.saturating_sub(weighting_started.elapsed()),
            ..limits
        },
        Some(&weights),
        |selected| {
            evaluation_number += 1;
            let Some((selection, physical)) = evaluate_configured_candidate(
                configured,
                context,
                selected,
                evaluation_number,
                &mut rejections,
            )?
            else {
                return Ok(None);
            };
            let cost = selection
                .costs
                .get(&selection.selected)
                .and_then(|cost| cost.cost_per_second)
                .ok_or("validated candidate has no scalar price")?;
            if best.as_ref().is_none_or(|(previous, ..)| cost < *previous) {
                best = Some((cost, selected.clone(), selection, physical));
            }
            Ok(Some(cost))
        },
    )?;
    match (&search.incumbent, &best) {
        (Some(incumbent), Some((cost, selected, ..)))
            if incumbent.actual_cost == *cost && incumbent.selected == *selected => {}
        (None, None) => {}
        _ => return Err("search incumbent and validated physical plan disagree".into()),
    }
    Ok(CandidatePlanningResult {
        search,
        rejections,
        best: best.map(|(_, _, selection, physical)| (selection, physical)),
    })
}

/// Prefer contracts that can survive downstream value computations. These are
/// proposal priorities, not feasibility rules: recomposition and evaluation
/// still decide the selected graph's actual accuracy.
fn accuracy_baseline_weights(
    configured: &CandidateConfigurationInventory,
    context: &CandidatePlanningContext<'_>,
) -> Result<Vec<f64>, String> {
    use asap_types::ir::properties::AccuracyMetric;
    use asap_types::ir::schema::FieldDataType;
    use asap_types::ir::ASAPOp;
    let graph = &configured.inventory.graph;
    graph.validate().map_err(|error| error.to_string())?;
    if configured.inventory.templates.len() != graph.candidates.len() {
        return Err("baseline templates must be indexed by candidate ID".into());
    }
    let mut output_queries = Vec::new();
    for (query, root) in context.original_roots.iter().enumerate() {
        match root {
            QueryRoot::Operator(_) => output_queries.push(query),
            QueryRoot::Scalar(expression) => {
                expression.map_operator_refs(&mut |node| {
                    output_queries.push(query);
                    std::rc::Rc::clone(node)
                });
            }
        }
    }
    if output_queries.len() != graph.query_roots.len() {
        return Err("baseline output references do not match original query roots".into());
    }
    let mut rank_requested = vec![false; graph.candidates.len()];
    let mut pending: Vec<_> = graph
        .query_roots
        .iter()
        .zip(output_queries)
        .filter(|(_, query)| {
            context.entries[*query]
                .requirements
                .accuracy
                .accuracy_metric
                == Some(AccuracyMetric::Rank)
        })
        .flat_map(|(root, _)| root.compatible_candidates.iter().copied())
        .collect();
    while let Some(candidate) = pending.pop() {
        if !rank_requested[candidate.0] {
            rank_requested[candidate.0] = true;
            pending.extend(
                graph.candidates[candidate.0]
                    .producer_requirements
                    .iter()
                    .flat_map(|input| input.compatible_producers.iter().copied()),
            );
        }
    }
    // Bound the numerical range; these tiers are intentionally heuristic.
    let preference = (graph.candidates.len() as f64 + 1.0).min(1000.0);
    configured
        .inventory
        .templates
        .iter()
        .enumerate()
        .map(|(id, template)| {
            let node = template
                .as_ref()
                .ok_or("baseline candidate has no operator template")?;
            let penalty = if let Some(family) = node.asap().and_then(ASAPOp::produced_state) {
                if matches!(family, FieldDataType::ExactAggregate(..)) {
                    0.0
                } else {
                    preference
                }
            } else if let Some(guarantee) = &node.guarantee {
                if guarantee.is_exact() {
                    0.0
                } else if guarantee.spec.accuracy_metric == AccuracyMetric::Rank
                    && !rank_requested[id]
                {
                    preference * preference
                } else {
                    preference
                }
            } else if matches!(node.asap(), Some(ASAPOp::SummaryEstimate { .. })) {
                preference * preference
            } else {
                // Exact operators may have no template guarantee until their
                // selected input contracts are recomposed; do not penalize that.
                0.0
            };
            Ok(1.0 + penalty)
        })
        .collect()
}

fn evaluate_configured_candidate(
    configured: &CandidateConfigurationInventory,
    context: &CandidatePlanningContext<'_>,
    selected: &SelectedCandidateGraph,
    evaluation_number: usize,
    rejections: &mut CandidateRejectionSummary,
) -> Result<Option<(Selection, PhysicalCandidate)>, String> {
    let mut modes = vec![None; configured.configurations.len()];
    for id in &selected.selected_candidates {
        modes[id.0] = Some(configured.configurations[id.0].materialization);
    }
    // Only a typed infeasible assignment justifies a no-good cut. Missing
    // identity, incompatible contracts and unsupported rewrites stop search.
    let mut realized = match materialize_selected_candidates(
        &configured.inventory,
        selected,
        &modes,
        context.original_roots,
        context.entries,
        context.data,
        context.models.deployment_capabilities,
    ) {
        Ok(realized) => realized,
        Err(PhysicalCandidateError::RejectedAssignment(reason)) => {
            rejections.record("materialization_assignment".into(), &reason);
            return Ok(None);
        }
        Err(error) => return Err(error.to_string()),
    };
    for id in &selected.selected_candidates {
        let node = realized.nodes_by_candidate[id.0]
            .as_ref()
            .ok_or_else(|| format!("candidate {} has no physical node", id.0))?;
        if node.timing != Some(configured.configurations[id.0].timing) {
            return Err(format!(
                "candidate {} requested timing {:?}, Stage 2 produced {:?}",
                id.0, configured.configurations[id.0].timing, node.timing
            ));
        }
    }
    // Stage 2 leaves IDs to its caller. A nonempty, stable proposal label keeps
    // selected-plan references and viewer cost lookups consistent.
    realized.physical.id = format!("joint-plan-{evaluation_number}");
    let mut selector = PlanSelector::new(context.entries, context.data, context.models, false)
        .map_err(|e| e.to_string())?;
    selector.set_calibration(context.calibration);
    selector
        .offer(&realized.physical)
        .map_err(|e| e.to_string())?;
    match selector.finish() {
        Ok(result) => Ok(Some(result)),
        Err(SelectionError::NoValidCandidate(rejected)) => {
            for rejection in rejected {
                let category = serde_json::to_value(rejection.cause)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_else(|| format!("{:?}", rejection.cause));
                rejections.record(category, &rejection.reason);
            }
            Ok(None)
        }
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::candidate_configurations::enumerate_candidate_configurations;
    use super::super::candidate_inventory::build_candidate_inventory;
    use super::super::candidate_search::CandidateSearchStatus;
    use super::*;
    use asap_logical_asap_optimization::memo::candidates::{try_populate_pass1, CandidateOrigin};
    use asap_logical_asap_optimization::memo::state_groups::expand_concrete_summary_state_groups;
    use asap_logical_asap_optimization::memo::{LogicalMemo, MemoGroupId};
    use asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::test_support::{entry_with, lower_promql};
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::schema::{FieldDataType, SketchAlgorithm};
    use asap_types::ir::{ASAPOp, OperatorNode};
    use asap_types::workload::{
        DataArrival, Predictability, QueryRecurrence, RepeatedDemand, RepetitionInterval,
        SourceFacts,
    };
    use std::time::Duration;

    fn fixture() -> (
        CandidateConfigurationInventory,
        Vec<QueryRoot>,
        Vec<QueryWorkloadEntry>,
        DataWorkload,
    ) {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let roots: Vec<_> = [0.95, 0.99]
            .into_iter()
            .enumerate()
            .map(|(id, q)| {
                (
                    id,
                    QueryRoot::Operator(lower_promql(
                        &format!("quantile_over_time({q}, latency[5m])"),
                        target.clone(),
                    )),
                )
            })
            .collect();
        let initial = LogicalMemo::from_roots(&roots);
        let outputs: Vec<_> = (0..2)
            .map(|q| {
                MemoGroupId(
                    initial
                        .groups
                        .iter()
                        .position(|g| g.roots_of.contains(&q))
                        .unwrap() as u32,
                )
            })
            .collect();
        let generated = generate_per_computation_candidates(roots.clone()).unwrap();
        let mut memo =
            expand_concrete_summary_state_groups(try_populate_pass1(initial, &generated).unwrap())
                .unwrap()
                .memo;
        // Keep the real generated shared KLL readers to bound this evaluator
        // integration test; materialization configurations still remain choices.
        for candidates in &mut memo.candidates {
            let shared: Vec<_> = candidates.iter().filter(|candidate| candidate.origin == CandidateOrigin::SharedBuild && OperatorNode::reachable(&candidate.node).iter().any(|node|
                matches!(node.asap(), Some(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. }) if *kind.algorithm() == SketchAlgorithm::Kll)))
                .cloned().collect();
            if !shared.is_empty() {
                *candidates = shared;
            }
        }
        let source = build_candidate_inventory(&memo, &outputs, |_| Ok(0.0)).unwrap();
        let entries: Vec<_> = (0..2)
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
            .into_iter()
            .collect(),
        };
        let configured = enumerate_candidate_configurations(
            &source,
            &[0, 1],
            &entries,
            &data,
            PlanningModels::builtin().deployment_capabilities,
        )
        .unwrap();
        (
            configured,
            roots.into_iter().map(|(_, root)| root).collect(),
            entries,
            data,
        )
    }

    // Rank-only quantiles cannot feed value arithmetic, and approximate values
    // cannot establish an exact top-k ranking; contract-aware priorities find a
    // validated seed without changing the feasible candidate catalog.
    #[test]
    fn accuracy_baseline_validates_composed_value_and_topk_workload() {
        let target = AccuracyTarget::max_error(0.05).failure_probability(0.3);
        let roots: Vec<_> = [
            "avg(quantile_over_time(0.95, latency[5m]))",
            "topk by (job) (3, sum_over_time(requests[5m]))",
        ]
        .into_iter()
        .enumerate()
        .map(|(id, query)| (id, QueryRoot::Operator(lower_promql(query, target.clone()))))
        .collect();
        let initial = LogicalMemo::from_roots(&roots);
        let outputs: Vec<_> = (0..roots.len())
            .map(|query| {
                MemoGroupId(
                    initial
                        .groups
                        .iter()
                        .position(|group| group.roots_of.contains(&query))
                        .unwrap() as u32,
                )
            })
            .collect();
        let generated = generate_per_computation_candidates(roots.clone()).unwrap();
        let mut memo = try_populate_pass1(initial, &generated).unwrap();
        let entries: Vec<_> = (0..roots.len())
            .map(|_| {
                let mut entry = entry_with(
                    target.clone(),
                    QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
                        1000,
                    ))),
                );
                entry.predictability = Predictability::Predictable { known_at: None };
                entry
            })
            .collect();
        crate::search::memo::rules::windows::add_local_window_composition(&mut memo, &entries);
        let memo = expand_concrete_summary_state_groups(memo).unwrap().memo;
        let source = build_candidate_inventory(&memo, &outputs, |_| Ok(0.0)).unwrap();
        let deployment = asap_logical_asap_optimization::DeploymentCapabilities {
            raw_data_retained: false,
            ..Default::default()
        };
        let data = DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 1000.0,
            },
            sources: ["latency", "requests"]
                .into_iter()
                .map(|name| {
                    (
                        name.into(),
                        SourceFacts {
                            series_count: Some(10),
                            values_non_negative: true,
                            ..Default::default()
                        },
                    )
                })
                .collect(),
        };
        let models = PlanningModels {
            deployment_capabilities: &deployment,
            ..PlanningModels::builtin()
        };
        let configured =
            enumerate_candidate_configurations(&source, &[0, 1], &entries, &data, &deployment)
                .unwrap();
        let roots: Vec<_> = roots.into_iter().map(|(_, root)| root).collect();
        let context = CandidatePlanningContext {
            original_roots: &roots,
            entries: &entries,
            data: &data,
            models,
            calibration: CalibrationFit::default(),
        };
        let limits = CandidateSearchLimits {
            max_refinements: 1,
            wall_time: Duration::from_secs(20),
        };
        let mut rejected = CandidateRejectionSummary::default();
        let plain = super::super::candidate_search::search_candidates_with_baseline(
            &configured.inventory.graph,
            limits,
            |selected| {
                Ok(evaluate_configured_candidate(
                    &configured,
                    &context,
                    selected,
                    1,
                    &mut rejected,
                )?
                .map(|(selection, _)| {
                    selection.costs[&selection.selected]
                        .cost_per_second
                        .unwrap()
                }))
            },
        )
        .unwrap();
        assert!(
            plain.incumbent.is_none(),
            "fixture must expose node-count baseline's missing contract"
        );
        assert!(rejected.counts.contains_key("no_accuracy_guarantee"));
        let result = optimize_configured_candidates(&configured, &context, limits).unwrap();
        assert!(
            result.best.is_some(),
            "{:?}: {:?}",
            result.search.status,
            result.rejections
        );
        assert_eq!(result.search.lower_bound, 0.0);
        assert_ne!(result.search.status, CandidateSearchStatus::Optimal);
    }

    // Explicit rank requirements remain valid baseline priorities, rather than
    // being treated as value-error requests or eliminating rank candidates.
    #[test]
    fn accuracy_baseline_respects_explicit_rank_targets() {
        use asap_types::ir::properties::AccuracyMetric;
        let (configured, roots, mut entries, data) = fixture();
        let weights = |entries: &[QueryWorkloadEntry]| {
            accuracy_baseline_weights(
                &configured,
                &CandidatePlanningContext {
                    original_roots: &roots,
                    entries,
                    data: &data,
                    models: PlanningModels::builtin(),
                    calibration: CalibrationFit::default(),
                },
            )
            .unwrap()
        };
        let value_weights = weights(&entries);
        for entry in &mut entries {
            entry.requirements.accuracy.accuracy_metric = Some(AccuracyMetric::Rank);
        }
        let rank_weights = weights(&entries);
        let rank_nodes: Vec<_> = configured
            .inventory
            .templates
            .iter()
            .enumerate()
            .filter(|(id, _)| {
                configured.inventory.graph.query_roots.iter().any(|root| {
                    root.compatible_candidates
                        .iter()
                        .any(|candidate| candidate.0 == *id)
                })
            })
            .filter_map(|(id, node)| {
                node.as_ref()
                    .and_then(|node| node.guarantee.as_ref())
                    .filter(|guarantee| {
                        !guarantee.is_exact()
                            && guarantee.spec.accuracy_metric == AccuracyMetric::Rank
                    })
                    .map(|_| id)
            })
            .collect();
        assert!(!rank_nodes.is_empty());
        assert!(rank_nodes
            .iter()
            .all(|&id| rank_weights[id] < value_weights[id]));
        assert!(configured
            .inventory
            .graph
            .candidates
            .iter()
            .all(|candidate| candidate.local_cost == 0.0));
    }

    // Both scalar price objectives use the full evaluator, with one frozen calibration.
    #[test]
    fn real_kll_search_cost_matches_independent_plan_selector_evaluation() {
        let (configured, roots, entries, data) = fixture();
        for provisioned in [false, true] {
            let mut settings = crate::PlanSelectionSettings::default();
            if provisioned {
                let CostModel::UsageOverTime(prices) = settings.cost_model.clone() else {
                    unreachable!()
                };
                settings.cost_model = CostModel::ProvisionedCapacity(prices);
            }
            let models = PlanningModels {
                plan_selection: &settings,
                ..PlanningModels::builtin()
            };
            let context = CandidatePlanningContext {
                original_roots: &roots,
                entries: &entries,
                data: &data,
                models,
                calibration: CalibrationFit::default(),
            };
            let result = optimize_configured_candidates(
                &configured,
                &context,
                CandidateSearchLimits {
                    max_refinements: 3,
                    wall_time: Duration::from_secs(20),
                },
            )
            .unwrap();
            let (selection, physical) = result
                .best
                .as_ref()
                .expect("search should retain a validated KLL plan");
            assert!(
                !selection.selected.is_empty(),
                "selected plan needs a viewer-visible ID"
            );
            assert_eq!(selection.selected, physical.id);
            let mut independent = PlanSelector::new(&entries, &data, models, false).unwrap();
            independent.set_calibration(context.calibration);
            independent.offer(physical).unwrap();
            let (reference, _) = independent.finish().unwrap();
            assert_eq!(
                selection.costs[&selection.selected].cost_per_second,
                reference.costs[&reference.selected].cost_per_second
            );
            assert_eq!(
                result.search.incumbent.as_ref().unwrap().actual_cost,
                reference.costs[&reference.selected]
                    .cost_per_second
                    .unwrap()
            );
            assert!(matches!(
                result.search.status,
                CandidateSearchStatus::RefinementLimit | CandidateSearchStatus::Optimal
            ));
        }
    }

    // A one-evaluation budget returns the validated full plan and an unfinished status.
    #[test]
    fn bounded_search_keeps_its_validated_plan() {
        let (configured, roots, entries, data) = fixture();
        let context = CandidatePlanningContext {
            original_roots: &roots,
            entries: &entries,
            data: &data,
            models: PlanningModels::builtin(),
            calibration: CalibrationFit::default(),
        };
        let result = optimize_configured_candidates(
            &configured,
            &context,
            CandidateSearchLimits {
                max_refinements: 1,
                wall_time: Duration::from_secs(20),
            },
        )
        .unwrap();
        assert!(result.best.is_some(), "{:?}", result.search.status);
        assert_eq!(result.search.status, CandidateSearchStatus::RefinementLimit);
        assert_eq!(result.search.refinements, 1);
        assert!(result.search.relative_gap.unwrap() > 0.0);
    }

    // Workload eligibility can reject one offered configuration; exclude only
    // that complete assignment and continue to a validated legal configuration.
    #[test]
    fn rejected_materialization_does_not_stop_search_for_legal_plan() {
        let (configured, roots, mut entries, data) = fixture();
        // The catalog was generated for predictable readers. This deliberately
        // exercises Stage 2's authoritative recheck on its ingestion proposal.
        entries[1].predictability = Predictability::AdHoc;
        let context = CandidatePlanningContext {
            original_roots: &roots,
            entries: &entries,
            data: &data,
            models: PlanningModels::builtin(),
            calibration: CalibrationFit::default(),
        };
        let result = optimize_configured_candidates(
            &configured,
            &context,
            CandidateSearchLimits {
                max_refinements: 20,
                wall_time: Duration::from_secs(20),
            },
        )
        .unwrap();
        assert_eq!(result.search.status, CandidateSearchStatus::Optimal);
        assert!(
            result.search.refinements >= 3,
            "both query modes and rejected ingestion must be evaluated"
        );
        assert!(
            result
                .rejections
                .counts
                .get("materialization_assignment")
                .copied()
                .unwrap_or(0)
                > 0
        );
        assert!(!result.rejections.examples.is_empty());
        let (_, physical) = result.best.unwrap();
        assert!(physical.materialization.values().all(|mode| !matches!(
            mode,
            asap_types::physical::Materialization::MatAtIngestion(_)
        )));
    }

    // A broken output contract is an adapter error, never evidence of infeasibility.
    #[test]
    fn unsupported_output_contract_stops_without_an_infeasibility_cut() {
        let (configured, mut roots, entries, data) = fixture();
        let QueryRoot::Operator(node) = &mut roots[0] else {
            unreachable!()
        };
        std::rc::Rc::make_mut(node).schema.fields[0]
            .name
            .push_str("_changed");
        let context = CandidatePlanningContext {
            original_roots: &roots,
            entries: &entries,
            data: &data,
            models: PlanningModels::builtin(),
            calibration: CalibrationFit::default(),
        };
        let result = optimize_configured_candidates(
            &configured,
            &context,
            CandidateSearchLimits {
                max_refinements: 20,
                wall_time: Duration::from_secs(20),
            },
        )
        .unwrap();
        assert!(matches!(
            result.search.status,
            CandidateSearchStatus::EvaluatorStopped(_)
        ));
        assert_eq!(result.search.refinements, 1);
        assert!(result.best.is_none());
    }

    // A cluster objective must not silently become scalar price optimization.
    #[test]
    fn cluster_capacity_objectives_are_explicitly_unsupported() {
        let (configured, roots, entries, data) = fixture();
        let settings = crate::PlanSelectionSettings {
            cost_model: CostModel::ClusterCapacity(crate::ClusterLimits {
                memory_bytes: u64::MAX,
                disk_bytes: u64::MAX,
                cpu_cores: None,
                objective: crate::ClusterObjective::ParetoFront,
            }),
            ..Default::default()
        };
        let context = CandidatePlanningContext {
            original_roots: &roots,
            entries: &entries,
            data: &data,
            models: PlanningModels {
                plan_selection: &settings,
                ..PlanningModels::builtin()
            },
            calibration: CalibrationFit::default(),
        };
        assert!(optimize_configured_candidates(
            &configured,
            &context,
            CandidateSearchLimits {
                max_refinements: 1,
                wall_time: Duration::from_secs(1)
            }
        )
        .err()
        .unwrap()
        .contains("ClusterCapacity"));
    }
}
