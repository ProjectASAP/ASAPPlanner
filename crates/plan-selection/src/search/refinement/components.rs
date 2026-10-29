//! Exact decomposition only when the candidate graph and evaluator are separable.
//! Potential dependencies, not just the incumbent's edges, determine components.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use asap_types::ir::{NonASAPOp, QueryRoot};

use crate::search::candidates::configurations::CandidateConfigurationInventory;
use crate::search::candidates::graph::{
    validate_selected_graph, CandidateDependencyGraph, PhysicalCandidateId,
    ProducerConsumerConnection, QueryRootAssignment, ReaderConstraint, SelectedCandidateGraph,
};
use crate::search::candidates::inventory::CandidateInventory;
use crate::search::candidates::suppliers::{SupplierCandidates, SupplierSetInterner};
use crate::search::milp::model::CandidateSolveMetrics;
#[cfg(test)]
use crate::search::refinement::planner::evaluate_configured_candidate;
use crate::search::refinement::planner::{
    evaluate_configured_candidate_before, optimize_configured_candidates, CandidatePlanningContext,
    CandidatePlanningResult, CandidateRejectionSummary,
};
use crate::search::refinement::search::{
    CandidateSearchLimits, CandidateSearchResult, CandidateSearchStatus, EvaluatedCandidate,
};
use crate::{CostModel, PlanSelector};

#[derive(Debug)]
struct Component {
    queries: Vec<usize>,
    candidates: Vec<usize>,
    outputs: Vec<usize>,
}

fn output_queries(roots: &[QueryRoot]) -> Vec<usize> {
    let mut outputs = Vec::new();
    for (query, root) in roots.iter().enumerate() {
        match root {
            QueryRoot::Operator(_) => outputs.push(query),
            QueryRoot::Scalar(expression) => {
                expression.map_operator_refs(&mut |node| {
                    outputs.push(query);
                    node.clone()
                });
            }
        }
    }
    outputs
}

fn components(
    configured: &CandidateConfigurationInventory,
    context: &CandidatePlanningContext<'_>,
) -> Result<Vec<Component>, String> {
    let graph = &configured.inventory.graph;
    graph.validate().map_err(|e| e.to_string())?;
    if configured.inventory.templates.len() != graph.candidates.len()
        || configured.configurations.len() != graph.candidates.len()
        || context.original_roots.len() != context.entries.len()
    {
        return Err("component inputs have inconsistent candidate/query indexing".into());
    }
    let outputs = output_queries(context.original_roots);
    if outputs.len() != graph.query_roots.len() {
        return Err("component output references do not match queries".into());
    }
    let count = context.entries.len();
    let whole = || {
        vec![Component {
            queries: (0..count).collect(),
            candidates: (0..graph.candidates.len()).collect(),
            outputs: (0..outputs.len()).collect(),
        }]
    };
    // Cluster capacities and provisioned billing couple peaks/rounding globally.
    // An external executor check can impose arbitrary workload-wide constraints.
    let CostModel::UsageOverTime(prices) = &context.models.plan_selection.cost_model else {
        return Ok(whole());
    };
    let builtin = crate::PlanningModels::builtin();
    if !graph.resource_budgets.is_empty()
        || !std::ptr::eq(context.models.accuracy, builtin.accuracy)
        || !std::ptr::eq(context.models.evidence, builtin.evidence)
    {
        return Ok(whole());
    }
    let mut parent: Vec<_> = (0..count).collect();
    fn find(parent: &mut [usize], mut id: usize) -> usize {
        while parent[id] != id {
            parent[id] = parent[parent[id]];
            id = parent[id];
        }
        id
    }
    fn join(parent: &mut [usize], a: usize, b: usize) {
        let a = find(parent, a);
        let b = find(parent, b);
        parent[b] = a;
    }
    let mut reach = vec![HashSet::new(); count];
    let mut first_reader = vec![None; graph.candidates.len()];
    let mut first_source = HashMap::<String, usize>::new();
    let raw_retention_priced = context.models.deployment_capabilities.raw_data_retained
        && prices.cost_per_disk_byte_second > 0.0;
    for (output, &query) in outputs.iter().enumerate() {
        let mut pending = graph.query_roots[output].compatible_candidates.clone();
        while let Some(id) = pending.pop() {
            if !reach[query].insert(id.0) {
                continue;
            }
            if let Some(other) = first_reader[id.0] {
                join(&mut parent, query, other);
            } else {
                first_reader[id.0] = Some(query);
            }
            if raw_retention_priced {
                let node = configured.inventory.templates[id.0]
                    .as_ref()
                    .ok_or("component candidate has no template")?;
                if let Some(NonASAPOp::Scan { source, .. }) = node.non_asap() {
                    let signature = serde_json::to_string(source).map_err(|e| e.to_string())?;
                    if let Some(&other) = first_source.get(&signature) {
                        join(&mut parent, query, other);
                    } else {
                        first_source.insert(signature, query);
                    }
                }
            }
            pending.extend(
                graph.candidates[id.0]
                    .producer_requirements
                    .iter()
                    .flat_map(|input| input.compatible_producers.iter().copied()),
            );
        }
    }
    for constraint in &graph.reader_constraints {
        if let ReaderConstraint::ActivationRequires {
            candidate,
            required_candidate,
        } = constraint
        {
            match (
                first_reader[candidate.0],
                first_reader[required_candidate.0],
            ) {
                (Some(left), Some(right)) => join(&mut parent, left, right),
                _ => return Ok(whole()),
            }
        }
    }
    let mut groups = BTreeMap::<usize, Vec<usize>>::new();
    for query in 0..count {
        groups
            .entry(find(&mut parent, query))
            .or_default()
            .push(query);
    }
    let mut result: Vec<_> = groups
        .into_values()
        .map(|queries| {
            let mut candidates: Vec<_> = queries
                .iter()
                .flat_map(|&q| reach[q].iter().copied())
                .collect();
            candidates.sort_unstable();
            candidates.dedup();
            let query_set: HashSet<_> = queries.iter().copied().collect();
            let selected_outputs = outputs
                .iter()
                .enumerate()
                .filter_map(|(id, query)| query_set.contains(query).then_some(id))
                .collect();
            Component {
                queries,
                candidates,
                outputs: selected_outputs,
            }
        })
        .collect();
    result.sort_by_key(|component| component.queries[0]);
    Ok(result)
}

/// Whether `component` holds every producer, root candidate and required copy
/// its candidates and outputs refer to, so `project` drops nothing.
fn closed(graph: &CandidateDependencyGraph, component: &Component) -> bool {
    let inside: HashSet<_> = component.candidates.iter().copied().collect();
    component.candidates.iter().all(|&id| {
        graph.candidates[id]
            .producer_requirements
            .iter()
            .flat_map(|input| input.compatible_producers.iter())
            .all(|producer| inside.contains(&producer.0))
    }) && component.outputs.iter().all(|&output| {
        graph.query_roots[output]
            .compatible_candidates
            .iter()
            .all(|id| inside.contains(&id.0))
    }) && graph
        .reader_constraints
        .iter()
        .all(|constraint| match constraint {
            ReaderConstraint::ActivationRequires {
                candidate,
                required_candidate,
            } => !inside.contains(&candidate.0) || inside.contains(&required_candidate.0),
            _ => true,
        })
}

fn project(
    configured: &CandidateConfigurationInventory,
    component: &Component,
) -> CandidateConfigurationInventory {
    let candidates: HashMap<_, _> = component
        .candidates
        .iter()
        .enumerate()
        .map(|(local, &global)| (global, local))
        .collect();
    let outputs: HashMap<_, _> = component
        .outputs
        .iter()
        .enumerate()
        .map(|(local, &global)| (global, local))
        .collect();
    let graph = &configured.inventory.graph;
    let mut supplier_sets = SupplierSetInterner::default();
    let mut remapped: HashMap<SupplierCandidates, SupplierCandidates> = HashMap::new();
    let mut projected = CandidateDependencyGraph {
        candidates: component
            .candidates
            .iter()
            .map(|&id| {
                let mut candidate = graph.candidates[id].clone();
                for input in &mut candidate.producer_requirements {
                    input.compatible_producers = remapped
                        .entry(input.compatible_producers.clone())
                        .or_insert_with(|| {
                            supplier_sets.intern(
                                input
                                    .compatible_producers
                                    .iter()
                                    .filter_map(|id| candidates.get(&id.0))
                                    .map(|&local| PhysicalCandidateId(local))
                                    .collect::<Vec<_>>(),
                            )
                        })
                        .clone();
                }
                candidate
            })
            .collect(),
        query_roots: component
            .outputs
            .iter()
            .map(|&id| {
                let mut root = graph.query_roots[id].clone();
                root.compatible_candidates = root
                    .compatible_candidates
                    .iter()
                    .filter_map(|id| candidates.get(&id.0))
                    .map(|&local| PhysicalCandidateId(local))
                    .collect();
                root
            })
            .collect(),
        ..Default::default()
    };
    for constraint in &graph.reader_constraints {
        let id = match constraint {
            ReaderConstraint::ActivationRequires { candidate, .. }
            | ReaderConstraint::AllowedOutputs { candidate, .. }
            | ReaderConstraint::IncompatibleOutputs { candidate, .. }
            | ReaderConstraint::HighestSharedQueryMaterialization { candidate, .. }
            | ReaderConstraint::MinimumQueryReaders { candidate, .. } => candidate.0,
        };
        let Some(&candidate) = candidates.get(&id) else {
            continue;
        };
        let candidate = PhysicalCandidateId(candidate);
        let mapped = match constraint {
            ReaderConstraint::ActivationRequires {
                required_candidate, ..
            } => match candidates.get(&required_candidate.0) {
                Some(&required) => ReaderConstraint::ActivationRequires {
                    candidate,
                    required_candidate: PhysicalCandidateId(required),
                },
                // The required copy is outside the component: never select it.
                None => ReaderConstraint::AllowedOutputs {
                    candidate,
                    outputs: Vec::new(),
                },
            },
            ReaderConstraint::AllowedOutputs {
                outputs: allowed, ..
            } => ReaderConstraint::AllowedOutputs {
                candidate,
                outputs: allowed
                    .iter()
                    .filter_map(|id| outputs.get(id).copied())
                    .collect(),
            },
            ReaderConstraint::IncompatibleOutputs { left, right, .. } => {
                let (Some(&left), Some(&right)) = (outputs.get(left), outputs.get(right)) else {
                    continue;
                };
                ReaderConstraint::IncompatibleOutputs {
                    candidate,
                    left,
                    right,
                }
            }
            ReaderConstraint::HighestSharedQueryMaterialization { query_outputs, .. } => {
                ReaderConstraint::HighestSharedQueryMaterialization {
                    candidate,
                    query_outputs: query_outputs
                        .iter()
                        .map(|bucket| {
                            bucket
                                .iter()
                                .filter_map(|id| outputs.get(id).copied())
                                .collect()
                        })
                        .collect(),
                }
            }
            ReaderConstraint::MinimumQueryReaders {
                query_outputs,
                minimum,
                ..
            } => ReaderConstraint::MinimumQueryReaders {
                candidate,
                query_outputs: query_outputs
                    .iter()
                    .map(|bucket| {
                        bucket
                            .iter()
                            .filter_map(|id| outputs.get(id).copied())
                            .collect()
                    })
                    .collect(),
                minimum: *minimum,
            },
        };
        projected.reader_constraints.push(mapped);
    }
    CandidateConfigurationInventory {
        inventory: CandidateInventory {
            graph: projected,
            templates: component
                .candidates
                .iter()
                .map(|&id| configured.inventory.templates[id].clone())
                .collect(),
            group_candidates: configured
                .inventory
                .group_candidates
                .iter()
                .map(|group| {
                    group
                        .iter()
                        .filter_map(|id| {
                            candidates
                                .get(&id.0)
                                .map(|&local| PhysicalCandidateId(local))
                        })
                        .collect()
                })
                .collect(),
        },
        configurations: component
            .candidates
            .iter()
            .map(|&id| configured.configurations[id])
            .collect(),
    }
}

fn extend_selection(
    global: &mut SelectedCandidateGraph,
    local: &SelectedCandidateGraph,
    component: &Component,
) {
    let candidate = |id: PhysicalCandidateId| PhysicalCandidateId(component.candidates[id.0]);
    global
        .selected_candidates
        .extend(local.selected_candidates.iter().copied().map(candidate));
    global.connections.extend(
        local
            .connections
            .iter()
            .map(|edge| ProducerConsumerConnection {
                producer: candidate(edge.producer),
                consumer: candidate(edge.consumer),
                requirement: edge.requirement,
            }),
    );
    global
        .query_roots
        .extend(local.query_roots.iter().map(|root| QueryRootAssignment {
            query_root: component.outputs[root.query_root],
            candidate: candidate(root.candidate),
        }));
    global.modeled_cost += local.modeled_cost;
}

/// The largest graph of shared candidates a query is searched over alone.
const MAX_SHARED_SEARCH_CANDIDATES: usize = 128;

/// Each query's own plan, searched for that query alone, combined into one
/// selection of the whole graph. A query is first searched over the
/// candidates no other query can reach; when they hold no plan (queries share
/// a group, such as one scan), over every candidate it can reach, when they
/// are at most [`MAX_SHARED_SEARCH_CANDIDATES`]. A candidate
/// two queries both select appears once: one node, as shared work is priced.
/// Returns it with the sum of the queries' own costs, or `None` when a query
/// finds no plan within `limits` (shared among the queries) or the
/// combination is not a valid selection. It is not yet evaluated for the
/// whole workload.
pub(in crate::search) fn separate_plans_combined(
    configured: &CandidateConfigurationInventory,
    context: &CandidatePlanningContext<'_>,
    limits: CandidateSearchLimits,
) -> Option<(SelectedCandidateGraph, f64)> {
    let graph = &configured.inventory.graph;
    let outputs = output_queries(context.original_roots);
    if outputs.len() != graph.query_roots.len() {
        return None;
    }
    let started = Instant::now();
    let count = context.entries.len();
    let mut separate_cost = 0.0;
    let mut combined = SelectedCandidateGraph {
        selected_candidates: Vec::new(),
        connections: Vec::new(),
        query_roots: Vec::new(),
        modeled_cost: 0.0,
    };
    let mut reach = vec![HashSet::new(); count];
    for (output, &query) in outputs.iter().enumerate() {
        let mut pending = graph.query_roots[output].compatible_candidates.clone();
        while let Some(id) = pending.pop() {
            if reach[query].insert(id.0) {
                pending.extend(
                    graph.candidates[id.0]
                        .producer_requirements
                        .iter()
                        .flat_map(|input| input.compatible_producers.iter().copied()),
                );
            }
        }
    }
    let mut readers = vec![0usize; graph.candidates.len()];
    for candidates in &reach {
        for &id in candidates {
            readers[id] += 1;
        }
    }
    for (query, reach) in reach.iter().enumerate() {
        let own_outputs: Vec<_> = (0..outputs.len())
            .filter(|&output| outputs[output] == query)
            .collect();
        let roots = [context.original_roots[query].clone()];
        let entries = [context.entries[query].clone()];
        let local_context = CandidatePlanningContext {
            original_roots: &roots,
            entries: &entries,
            seed_separate_plans: false,
            ..*context
        };
        let mut found = false;
        for own_only in [true, false] {
            let mut candidates: Vec<_> = reach
                .iter()
                .copied()
                .filter(|&id| !own_only || readers[id] == 1)
                .collect();
            if !own_only
                && (candidates.len() > MAX_SHARED_SEARCH_CANDIDATES
                    || candidates.len() == reach.iter().filter(|&&id| readers[id] == 1).count())
            {
                // Nothing new to search, or too large a graph to prove an
                // optimum within the budget: it would only take the joint
                // search's time.
                break;
            }
            candidates.sort_unstable();
            let component = Component {
                queries: vec![query],
                candidates,
                outputs: own_outputs.clone(),
            };
            let local = project(configured, &component);
            let Ok(result) = optimize_configured_candidates(
                &local,
                &local_context,
                CandidateSearchLimits {
                    wall_time: limits.wall_time.saturating_sub(started.elapsed())
                        / (count - query) as u32,
                    max_refinements: limits.max_refinements,
                },
            ) else {
                continue;
            };
            if let Some(incumbent) = result.search.incumbent {
                extend_selection(&mut combined, &incumbent.selected, &component);
                separate_cost += incumbent.actual_cost;
                found = true;
                break;
            }
        }
        if !found {
            return None;
        }
    }
    combined.selected_candidates.sort_unstable();
    combined.selected_candidates.dedup();
    let mut connections = Vec::with_capacity(combined.connections.len());
    for connection in combined.connections {
        if !connections.contains(&connection) {
            connections.push(connection);
        }
    }
    combined.connections = connections;
    combined.query_roots.sort_by_key(|root| root.query_root);
    // A shared candidate is counted once.
    combined.modeled_cost = combined
        .selected_candidates
        .iter()
        .map(|id| graph.candidates[id.0].local_cost)
        .sum();
    validate_selected_graph(graph, &combined).ok()?;
    Some((combined, separate_cost))
}

/// Split only additive usage optimization, then validate the reconstructed full
/// workload with the original evaluator. The same frozen calibration and data
/// facts apply to every component; query request overhead is charged per query.
/// Failure-probability sizing divides each root's budget across its own inputs
/// and covered values, then takes the minimum for shared state. Such state joins
/// components above; there is no workload-global failure-budget allocation.
pub(in crate::search) fn optimize_independent_components(
    configured: &CandidateConfigurationInventory,
    context: &CandidatePlanningContext<'_>,
    limits: CandidateSearchLimits,
) -> Result<CandidatePlanningResult, String> {
    PlanSelector::new(context.entries, context.data, context.models, false)
        .map_err(|e| e.to_string())?;
    let started = Instant::now();
    let context = &CandidatePlanningContext {
        deadline: crate::search::refinement::planner::earliest_deadline(
            context.deadline,
            started.checked_add(limits.wall_time),
        ),
        ..*context
    };
    let parts = components(configured, context)?;
    if parts.len() <= 1 {
        return optimize_configured_candidates(configured, context, limits);
    }
    // `project` drops what lies outside a component; components are closed.
    debug_assert!(parts
        .iter()
        .all(|part| closed(&configured.inventory.graph, part)));
    let mut combined = SelectedCandidateGraph {
        selected_candidates: Vec::new(),
        connections: Vec::new(),
        query_roots: Vec::new(),
        modeled_cost: 0.0,
    };
    let mut summary = CandidateSearchResult {
        incumbent: None,
        status: CandidateSearchStatus::Optimal,
        lower_bound: 0.0,
        relative_gap: None,
        refinements: 0,
        solve_metrics: CandidateSolveMetrics::default(),
        validation_seconds: 0.0,
        certified_query_cuts: 0,
    };
    let mut rejections = CandidateRejectionSummary::default();
    let mut sum_cost = 0.0;
    // Each component searches until its share of the time is spent, so an
    // eighth is kept to validate the combined plan afterwards; without it
    // the components use the whole budget and the combined plan, valid in
    // every component, misses the deadline and the search ends without a plan.
    let search_time = limits.wall_time.mul_f64(0.875);
    for (index, component) in parts.iter().enumerate() {
        let local = project(configured, component);
        let roots: Vec<_> = component
            .queries
            .iter()
            .map(|&id| context.original_roots[id].clone())
            .collect();
        let entries: Vec<_> = component
            .queries
            .iter()
            .map(|&id| context.entries[id].clone())
            .collect();
        let local_context = CandidatePlanningContext {
            deadline: context.deadline,
            refine_materialization: context.refine_materialization,
            seed_separate_plans: context.seed_separate_plans,
            original_roots: &roots,
            entries: &entries,
            data: context.data,
            models: context.models,
            calibration: context.calibration,
        };
        let remaining_parts = parts.len() - index;
        let result = optimize_configured_candidates(
            &local,
            &local_context,
            CandidateSearchLimits {
                wall_time: search_time.saturating_sub(started.elapsed()) / remaining_parts as u32,
                max_refinements: limits.max_refinements.saturating_sub(summary.refinements)
                    / remaining_parts,
            },
        )?;
        summary.lower_bound += result.search.lower_bound;
        summary.refinements += result.search.refinements;
        summary.certified_query_cuts += result.search.certified_query_cuts;
        summary.validation_seconds += result.search.validation_seconds;
        let metrics = &result.search.solve_metrics;
        summary.solve_metrics.model_construction_seconds += metrics.model_construction_seconds;
        summary.solve_metrics.solver_seconds += metrics.solver_seconds;
        summary.solve_metrics.solve_calls += metrics.solve_calls;
        summary.solve_metrics.direct_cost_solve_calls += metrics.direct_cost_solve_calls;
        summary.solve_metrics.joint_price_solve_calls += metrics.joint_price_solve_calls;
        summary.solve_metrics.direct_cost_validated_plans += metrics.direct_cost_validated_plans;
        // Component proposal costs are not complete workload costs; do not
        // expose their minimum as the price of a validated whole-workload plan.
        for reason in &metrics.direct_cost_fallback_reasons {
            if summary.solve_metrics.direct_cost_fallback_reasons.len() < 8
                && !summary
                    .solve_metrics
                    .direct_cost_fallback_reasons
                    .contains(reason)
            {
                summary
                    .solve_metrics
                    .direct_cost_fallback_reasons
                    .push(reason.clone());
            }
        }
        summary.solve_metrics.peak_variables = summary
            .solve_metrics
            .peak_variables
            .max(metrics.peak_variables);
        summary.solve_metrics.peak_constraints = summary
            .solve_metrics
            .peak_constraints
            .max(metrics.peak_constraints);
        for (cause, count) in result.rejections.counts {
            *rejections.counts.entry(cause).or_default() += count;
        }
        for example in result.rejections.examples {
            if rejections.examples.len() < 8 {
                rejections
                    .examples
                    .push(format!("queries {:?}: {example}", component.queries));
            }
        }
        if result.search.status != CandidateSearchStatus::Optimal {
            summary.status = result.search.status;
        }
        let Some(incumbent) = result.search.incumbent else {
            return Ok(CandidatePlanningResult {
                independent_components: parts.len(),
                search: summary,
                rejections,
                best: None,
            });
        };
        sum_cost += incumbent.actual_cost;
        extend_selection(&mut combined, &incumbent.selected, component);
    }
    combined.selected_candidates.sort_unstable();
    combined.query_roots.sort_by_key(|root| root.query_root);
    validate_selected_graph(&configured.inventory.graph, &combined)?;
    let validation_started = Instant::now();
    let mut rejected_queries = Vec::new();
    let evaluated = evaluate_configured_candidate_before(
        configured,
        context,
        &combined,
        1,
        &mut rejections,
        &mut rejected_queries,
    );
    let best = match evaluated {
        Ok(best) => best.ok_or("independent component plans failed full-workload validation")?,
        Err(crate::SelectionError::EvaluationDeadlineExceeded) => {
            summary.validation_seconds += validation_started.elapsed().as_secs_f64();
            summary.status = CandidateSearchStatus::TimeLimit;
            return Ok(CandidatePlanningResult {
                independent_components: parts.len(),
                search: summary,
                rejections,
                best: None,
            });
        }
        Err(error) => return Err(error.to_string()),
    };
    summary.validation_seconds += validation_started.elapsed().as_secs_f64();
    let cost = best.0.costs[&best.0.selected]
        .cost_per_second
        .ok_or("combined plan has no scalar cost")?;
    if (cost - sum_cost).abs() > 1e-8 * cost.abs().max(sum_cost.abs()) {
        return Err(format!(
            "component costs are not additive: component sum {sum_cost}, full workload {cost}"
        ));
    }
    summary.relative_gap = Some(if cost == 0.0 {
        0.0
    } else {
        ((cost - summary.lower_bound) / cost).max(0.0)
    });
    summary.incumbent = Some(EvaluatedCandidate {
        selected: combined,
        actual_cost: cost,
    });
    Ok(CandidatePlanningResult {
        independent_components: parts.len(),
        search: summary,
        rejections,
        best: Some(best),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::candidates::configurations::enumerate_candidate_configurations;
    use crate::search::candidates::graph::{
        PhysicalCandidate, QueryRootCandidates, ResourceBudget,
    };
    use crate::search::milp::model::{solve_candidate_proposal, ObservedAssignment};
    use crate::{CalibrationFit, PlanSelectionSettings, PlanningModels, ResourceUnitCost};
    use asap_logical_asap_optimization::pass2::memo::MemoGroupId;
    use asap_logical_asap_optimization::test_support::{entry_with, lower_promql};
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::scalar::{ScalarExpr, ScalarValue};
    use asap_types::ir::{Operator, OperatorNode};
    use asap_types::workload::*;
    use std::rc::Rc;
    use std::time::Duration;

    fn fixture(
        literal: bool,
    ) -> (
        CandidateConfigurationInventory,
        Vec<QueryRoot>,
        Vec<QueryWorkloadEntry>,
        DataWorkload,
    ) {
        let scans: Vec<_> = ["a", "b"]
            .into_iter()
            .map(|metric| {
                OperatorNode::reachable(&lower_promql(metric, AccuracyTarget::exact()))
                    .into_iter()
                    .find(|node| matches!(node.non_asap(), Some(NonASAPOp::Scan { .. })))
                    .unwrap()
            })
            .collect();
        let source = CandidateInventory {
            graph: CandidateDependencyGraph {
                candidates: (0..2)
                    .map(|id| PhysicalCandidate {
                        group: MemoGroupId(id),
                        local_cost: 0.0,
                        producer_requirements: Vec::new(),
                    })
                    .collect(),
                query_roots: [0, 0, 1, 1]
                    .into_iter()
                    .map(|id| QueryRootCandidates {
                        compatible_candidates: vec![PhysicalCandidateId(id)],
                    })
                    .collect(),
                ..Default::default()
            },
            templates: scans.iter().map(|node| Some(Rc::clone(node))).collect(),
            group_candidates: Vec::new(),
        };
        let mut roots: Vec<_> = [0, 0, 1, 1]
            .into_iter()
            .map(|id| QueryRoot::Operator(Rc::clone(&scans[id])))
            .collect();
        if literal {
            roots.push(QueryRoot::Scalar(ScalarExpr::Literal(
                ScalarValue::Float64(7.0),
            )));
        }
        let entries: Vec<_> = roots
            .iter()
            .map(|_| {
                entry_with(
                    AccuracyTarget::exact(),
                    QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
                        1000,
                    ))),
                )
            })
            .collect();
        let data = DataWorkload {
            sources: ["a", "b"]
                .into_iter()
                .map(|metric| {
                    (
                        metric.into(),
                        SourceFacts {
                            rows_held: Some(100),
                            series_count: Some(2),
                            ..Default::default()
                        },
                    )
                })
                .collect(),
            ..Default::default()
        };
        let configured = enumerate_candidate_configurations(
            &source,
            &[0, 1, 2, 3],
            &entries,
            &data,
            PlanningModels::builtin().deployment_capabilities,
        )
        .unwrap();
        (configured, roots, entries, data)
    }

    // Exhaustive full-graph evaluation agrees with separate solves, including
    // per-request fees for an output with no operator and fixed measured calibration.
    #[test]
    fn independent_components_match_unsplit_exhaustive_optimum() {
        let (configured, roots, entries, data) = fixture(true);
        let mut settings = PlanSelectionSettings::default();
        settings.measured_operation_costs.ns_per_evaluation = 2_000_000.0;
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            seed_separate_plans: false,
            original_roots: &roots,
            entries: &entries,
            data: &data,
            models: PlanningModels::builtin().with_plan_selection(&settings),
            calibration: CalibrationFit {
                measured_ms_per_second: 2.0,
                formula_ms_per_second: 1.0,
            },
        };
        let parts = components(&configured, &context).unwrap();
        assert_eq!(
            parts
                .iter()
                .map(|component| component.queries.clone())
                .collect::<Vec<_>>(),
            vec![vec![0, 1], vec![2, 3], vec![4]]
        );
        let mut observed = Vec::new();
        let mut optimum = f64::INFINITY;
        while let Some(proposal) =
            solve_candidate_proposal(&configured.inventory.graph, &observed, None).unwrap()
        {
            let result = evaluate_configured_candidate(
                &configured,
                &context,
                &proposal.selected,
                1,
                &mut CandidateRejectionSummary::default(),
                &mut Vec::new(),
            )
            .unwrap()
            .unwrap();
            let cost = result.0.costs[&result.0.selected].cost_per_second.unwrap();
            optimum = optimum.min(cost);
            observed.push(ObservedAssignment {
                selected: proposal.selected,
                exact_cost: None,
                rejected_queries: Vec::new(),
            });
            assert!(observed.len() <= 4);
        }
        assert_eq!(observed.len(), 4);
        let split = optimize_independent_components(
            &configured,
            &context,
            CandidateSearchLimits {
                max_refinements: 48,
                wall_time: Duration::from_secs(20),
            },
        )
        .unwrap();
        let incumbent = split.search.incumbent.unwrap();
        assert!((incumbent.actual_cost - optimum).abs() <= 1e-9 * optimum);
        validate_selected_graph(&configured.inventory.graph, &incumbent.selected).unwrap();
        let (selection, physical) = split.best.unwrap();
        assert_eq!(physical.roots.len(), 5);
        assert_eq!(selection.accepted[&selection.selected].len(), 5);
    }

    // Potential sharing and source retention must couple decisions even when an
    // incumbent would happen to choose disjoint nodes. Global budgets never split.
    #[test]
    fn source_retention_alternative_dependencies_and_global_limits_prevent_split() {
        let (mut configured, roots, entries, data) = fixture(false);
        let mut retention_settings = PlanSelectionSettings::default();
        let CostModel::UsageOverTime(prices) = &mut retention_settings.cost_model else {
            unreachable!()
        };
        prices.cost_per_disk_byte_second = 1e-9;
        let models = PlanningModels::builtin().with_plan_selection(&retention_settings);
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            seed_separate_plans: false,
            original_roots: &roots,
            entries: &entries,
            data: &data,
            models,
            calibration: CalibrationFit::default(),
        };
        assert_eq!(components(&configured, &context).unwrap().len(), 2);
        let first = configured.inventory.graph.query_roots[0].compatible_candidates[0];
        configured.inventory.graph.query_roots[2]
            .compatible_candidates
            .push(first);
        assert_eq!(components(&configured, &context).unwrap().len(), 1);
        configured.inventory.graph.query_roots[2]
            .compatible_candidates
            .pop();
        for node in configured.inventory.templates.iter_mut().flatten() {
            let mut changed = (**node).clone();
            let Operator::NonASAP(NonASAPOp::Scan { source, .. }) = &mut changed.operator else {
                unreachable!()
            };
            *source = asap_types::ir::operator::Source::TimeSeries { metric: "a".into() };
            *node = Rc::new(changed);
        }
        assert_eq!(components(&configured, &context).unwrap().len(), 1);
        let mut free_disk = PlanSelectionSettings::default();
        let CostModel::UsageOverTime(prices) = &mut free_disk.cost_model else {
            unreachable!()
        };
        prices.cost_per_disk_byte_second = 0.0;
        let independent = CandidatePlanningContext {
            deadline: None,
            models: models.with_plan_selection(&free_disk),
            ..context
        };
        assert_eq!(components(&configured, &independent).unwrap().len(), 2);
        configured
            .inventory
            .graph
            .resource_budgets
            .push(ResourceBudget {
                name: "global memory".into(),
                capacity: 100.0,
                candidate_usage: vec![1.0; configured.configurations.len()],
            });
        assert_eq!(components(&configured, &independent).unwrap().len(), 1);
        configured.inventory.graph.resource_budgets.clear();
        let provisioned = PlanSelectionSettings {
            cost_model: CostModel::ProvisionedCapacity(ResourceUnitCost::illustrative()),
            ..Default::default()
        };
        let provisioned = CandidatePlanningContext {
            deadline: None,
            models: models.with_plan_selection(&provisioned),
            ..independent
        };
        assert_eq!(components(&configured, &provisioned).unwrap().len(), 1);
    }
}
