//! Evaluator-backed refinement over configured candidate connections.
//!
//! Every proposal passes Stage 2 and PlanSelector; only a validated price
//! becomes an exact refinement cut.
use asap_types::ir::QueryRoot;
use asap_types::physical::PhysicalWorkloadPlan;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};
use std::collections::BTreeMap;

use crate::search::candidates::dag::PhysicalCandidatesWithCosts;
use crate::search::candidates::graph::RejectedQuerySubgraph;
use crate::search::candidates::graph::SelectedCandidateGraph;
use crate::search::refinement::search::{
    search_candidates_with_direct_feedback, CandidateEvaluation, CandidateSearchLimits,
    CandidateSearchResult, DirectPriceModel,
};
use crate::{ErrorSelectingPlan, PlanSelector, SelectionModels, WholePlanSelection};
use asap_cost::{CalibrationFit, CostModel};
use asap_types::stages::{AssignmentRealizer, PhysicalCandidateError};

#[derive(Clone, Copy)]
pub(in crate::search) struct CandidatePlanningContext<'a> {
    pub deadline: Option<std::time::Instant>,
    pub refine_materialization: bool,
    /// The workload's query roots as Stage 3 sees them: each output
    /// reference is an operator tree of its group read from Stage 2's
    /// candidates ([`query_roots_of`](crate::search::candidates::dag::query_roots_of)).
    pub query_roots: &'a [QueryRoot],
    /// Stage 2's realizer, over the whole workload.
    pub realizer: &'a dyn AssignmentRealizer,
    pub entries: &'a [QueryWorkloadEntry],
    pub data: &'a DataWorkload,
    pub models: SelectionModels<'a>,
    /// One externally established fit, held constant across every proposal.
    pub calibration: CalibrationFit,
}

pub(in crate::search) fn earliest_deadline(
    outer: Option<std::time::Instant>,
    inner: Option<std::time::Instant>,
) -> Option<std::time::Instant> {
    match (outer, inner) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Bounded evidence explaining rejected assignments, not complete plan history.
#[derive(Debug, Default)]
pub(in crate::search) struct CandidateRejectionSummary {
    pub counts: BTreeMap<String, usize>,
    pub examples: Vec<String>,
}

impl CandidateRejectionSummary {
    pub(in crate::search) fn record(&mut self, category: String, reason: &str) {
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

// Read by the engine's tests; #1095 replaces the engine.
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::search) struct CandidatePlanningResult {
    pub search: CandidateSearchResult,
    pub rejections: CandidateRejectionSummary,
    /// Same convention as PlanSelector::finish: WholePlanSelection.plan is the validated
    /// execution plan; PhysicalWorkloadPlan is the offered Stage 2 candidate.
    pub best: Option<(WholePlanSelection, PhysicalWorkloadPlan)>,
}

/// Usage and provisioned-price objectives use the complete evaluator cost,
/// including held memory, raw retention and request overhead where applicable.
/// Cluster objectives use candidate_objectives and the original selector
/// archive; their Pareto/lexicographic policy is not a scalar price.
pub(in crate::search) fn optimize_configured_candidates(
    configured: &PhysicalCandidatesWithCosts,
    context: &CandidatePlanningContext<'_>,
    limits: CandidateSearchLimits,
) -> Result<CandidatePlanningResult, String> {
    if !matches!(
        context.models.plan_selection.cost_model,
        CostModel::UsageOverTime(_) | CostModel::ProvisionedCapacity(_)
    ) {
        return Err("candidate price search does not support ClusterCapacity objectives".into());
    }
    if configured.physical.candidates.len() != configured.dag.candidates.len() {
        return Err("physical configurations must be indexed by candidate ID".into());
    }
    if context.query_roots.len() != context.entries.len() {
        return Err("query roots and workload entries differ in length".into());
    }
    // Reject invalid settings/deployment even if the search budget is zero.
    PlanSelector::new(context.entries, context.data, context.models, false)
        .map_err(|e| e.to_string())?;
    if context
        .models
        .plan_selection
        .frozen_calibration_factor
        .is_some_and(|factor| factor != context.calibration.factor())
    {
        return Err(
            "candidate cost bounds and evaluator must use the same frozen calibration factor"
                .into(),
        );
    }
    if !context.calibration.factor().is_finite() || context.calibration.factor() <= 0.0 {
        return Err("candidate search requires a finite positive frozen calibration".into());
    }
    let local_deadline = std::time::Instant::now().checked_add(limits.wall_time);
    let context = &CandidatePlanningContext {
        deadline: earliest_deadline(context.deadline, local_deadline),
        ..*context
    };
    let mut best: Option<(
        f64,
        SelectedCandidateGraph,
        WholePlanSelection,
        PhysicalWorkloadPlan,
    )> = None;
    let mut evaluation_number = 0;
    let mut rejections = CandidateRejectionSummary::default();
    let weighting_started = std::time::Instant::now();
    let (weights, exclusions) = accuracy_baseline(configured, context)?;
    let changes = if context.refine_materialization {
        super::materialization::counterparts(configured, context.deadline)
    } else {
        Vec::new()
    };
    let mut joint_fallback_reasons = Vec::new();
    let joint_context = CandidatePlanningContext {
        deadline: earliest_deadline(
            context.deadline,
            std::time::Instant::now()
                .checked_add((limits.wall_time / 6).min(std::time::Duration::from_secs(2))),
        ),
        ..*context
    };
    let joint = joint_cost_seed(
        configured,
        &joint_context,
        &exclusions,
        &mut joint_fallback_reasons,
    )?;
    let mut direct = Vec::new();
    // Use a fixed-interface profile as a bounded fallback when the
    // joint interface catalog cannot be certified within the planning budget.
    // Both adapters share the same construction deadline, preserving time for
    // the feasibility search even when direct pricing is unsupported.
    if joint.is_none() {
        if let Some(seed) = direct_cost_seed(configured, &joint_context, &exclusions, false)? {
            direct.push(seed);
        }
        if let Some(seed) = direct_cost_seed(configured, &joint_context, &exclusions, true)? {
            direct.push(seed);
        }
    }
    // A maintained-state seed avoids the large raw-scan latency range without
    // weakening the direct model's numerical guards. It only proposes plans;
    // the unrestricted graph and evaluator still determine bounds and validity.
    let maintained = joint
        .as_ref()
        .filter(|(_, profile)| profile.schedules.is_some())
        .and_then(|(_, profile)| maintained_state_profile(configured, profile, context.deadline));
    // If even this smaller profile has unrepresentable monetary coefficients,
    // its supported paths can still provide a structural feasibility seed.
    let maintained_graph = maintained
        .as_ref()
        .map(|profile| {
            use asap_types::stages::{PhysicalCandidateId, ReaderConstraint};
            let mut graph = configured.dag.clone();
            for (id, candidate) in graph.candidates.iter_mut().enumerate() {
                candidate.local_cost = weights[id];
                if profile.candidates[id].is_empty() {
                    candidate.producer_candidates_per_input.clear();
                    graph
                        .reader_constraints
                        .push(ReaderConstraint::AllowedOutputs {
                            candidate: PhysicalCandidateId(id),
                            outputs: Vec::new(),
                        });
                } else {
                    for input in &mut candidate.producer_candidates_per_input {
                        input
                            .candidates
                            .retain(|producer| !profile.candidates[producer.0].is_empty());
                    }
                }
            }
            for root in &mut graph.query_roots {
                root.candidates
                    .retain(|id| !profile.candidates[id.0].is_empty());
            }
            graph
        })
        .filter(|graph| {
            graph
                .query_roots
                .iter()
                .all(|root| !root.candidates.is_empty())
        });
    let mut direct_refs: Vec<_> = direct
        .iter()
        .map(|(graph, profile)| (graph, DirectPriceModel::Fixed(profile)))
        .collect();
    if let (Some((graph, _)), Some(profile)) = (&joint, &maintained) {
        direct_refs.push((graph, DirectPriceModel::Joint(profile)));
    }
    if let Some(graph) = &maintained_graph {
        direct_refs.push((graph, DirectPriceModel::Structural));
    }
    if let Some((graph, profile)) = &joint {
        direct_refs.push((graph, DirectPriceModel::Joint(profile)));
    }
    let mut evaluate = |selected: &SelectedCandidateGraph| -> Result<CandidateEvaluation, String> {
        evaluation_number += 1;
        let mut rejected_queries = Vec::new();
        let evaluated = evaluate_configured_candidate_before(
            configured,
            context,
            selected,
            evaluation_number,
            &mut rejections,
            &mut rejected_queries,
        );
        let Some((selection, physical)) = (match evaluated {
            Ok(result) => result,
            Err(ErrorSelectingPlan::EvaluationDeadlineExceeded) => {
                return Ok(CandidateEvaluation {
                    budget_exhausted: true,
                    ..Default::default()
                });
            }
            Err(
                error @ ErrorSelectingPlan::Cost {
                    error: crate::AnalyticalCostError::Overflow,
                    ..
                },
            ) => {
                let reason = error.to_string();
                rejections.record("cost_not_representable".into(), &reason);
                return Ok(CandidateEvaluation {
                    unpriced_reason: Some(reason),
                    ..Default::default()
                });
            }
            Err(error) => return Err(error.to_string()),
        }) else {
            return Ok(CandidateEvaluation {
                actual_cost: None,
                rejected_queries,
                budget_exhausted: false,
                unpriced_reason: None,
            });
        };
        let cost = selection
            .costs
            .get(&selection.selected)
            .and_then(|cost| cost.cost_per_second)
            .ok_or("validated candidate has no scalar price")?;
        if best.as_ref().is_none_or(|(previous, ..)| cost < *previous) {
            best = Some((cost, selected.clone(), selection, physical));
        }
        Ok(Some(cost).into())
    };
    let mut search = search_candidates_with_direct_feedback(
        &configured.dag,
        CandidateSearchLimits {
            wall_time: limits.wall_time.saturating_sub(weighting_started.elapsed()),
            ..limits
        },
        &weights,
        &exclusions,
        &changes,
        &direct_refs,
        &mut evaluate,
    )?;
    search
        .solve_metrics
        .direct_cost_fallback_reasons
        .extend(joint_fallback_reasons);
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

/// Every supported local price interface participates in one joint solve.
/// Keep paths whose scans run at ingestion time and whose query readouts are
/// not materialized. This avoids tiny held-memory charges on scalar readouts
/// spanning a much smaller scale than ingestion costs. Prune unsupported price
/// tuples to a fixed point before compacting option indices.
fn maintained_state_profile(
    configured: &PhysicalCandidatesWithCosts,
    profile: &crate::search::milp::direct_price_options::JointPriceProfile,
    deadline: Option<std::time::Instant>,
) -> Option<crate::search::milp::direct_price_options::JointPriceProfile> {
    let mut supported: Vec<Vec<bool>> = profile
        .candidates
        .iter()
        .enumerate()
        .map(|(id, options)| {
            let scan = matches!(
                configured.node(id).non_asap(),
                Some(asap_types::ir::NonASAPOp::Scan { .. })
            );
            options
                .iter()
                .map(|option| {
                    (!scan || option.profile.ingestion)
                        && (option.profile.ingestion || !option.profile.materialized)
                })
                .collect()
        })
        .collect();
    loop {
        let mut changed = false;
        for (id, options) in profile.candidates.iter().enumerate() {
            if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
                return None;
            }
            for (index, option) in options.iter().enumerate() {
                if supported[id][index]
                    && !option.compatible_input_tuples.iter().any(|tuple| {
                        tuple
                            .iter()
                            .all(|binding| supported[binding.producer.0][binding.price_option])
                    })
                {
                    supported[id][index] = false;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    let remap: Vec<Vec<Option<usize>>> = supported
        .iter()
        .map(|options| {
            let mut next = 0;
            options
                .iter()
                .map(|keep| {
                    if *keep {
                        let index = next;
                        next += 1;
                        Some(index)
                    } else {
                        None
                    }
                })
                .collect()
        })
        .collect();
    let mut result = profile.clone();
    for (id, options) in result.candidates.iter_mut().enumerate() {
        let mut index = 0;
        options.retain_mut(|option| {
            let keep = supported[id][index];
            index += 1;
            if keep {
                option.compatible_input_tuples.retain(|tuple| {
                    tuple
                        .iter()
                        .all(|binding| supported[binding.producer.0][binding.price_option])
                });
                for tuple in &mut option.compatible_input_tuples {
                    for binding in tuple {
                        binding.price_option =
                            remap[binding.producer.0][binding.price_option].unwrap();
                    }
                }
            }
            keep
        });
    }
    Some(result)
}

fn joint_cost_seed(
    configured: &PhysicalCandidatesWithCosts,
    context: &CandidatePlanningContext<'_>,
    exclusions: &[asap_types::stages::PhysicalCandidateId],
    fallback_reasons: &mut Vec<String>,
) -> Result<
    Option<(
        crate::search::candidates::dag::DagOfPhysicalCandidatesWithCosts,
        crate::search::milp::direct_price_options::JointPriceProfile,
    )>,
    String,
> {
    use crate::search::costing::direct_profile::{extract_price_options, DirectProfileError};
    let mut profile = match extract_price_options(configured, context) {
        Ok(Some(profile)) => profile,
        Ok(None) => return Ok(None),
        Err(DirectProfileError::DeadlineExceeded(_)) => {
            fallback_reasons
                .push("joint pricing: interface generation exhausted its reserved budget".into());
            return Ok(None);
        }
        Err(DirectProfileError::Unsupported(reason)) => {
            fallback_reasons.push(format!("joint pricing: {reason}"));
            return Ok(None);
        }
        Err(error) => return Err(error.to_string()),
    };
    // As with the fixed-price seed, avoid permutations of interchangeable
    // execution copies in this bounded proposal. Every producer price option
    // remains available on the first copy; the full search retains all copies.
    for constraint in &configured.dag.reader_constraints {
        if let asap_types::stages::ReaderConstraint::ActivationRequires { candidate, .. } =
            constraint
        {
            profile.candidates[candidate.0].clear();
        }
    }
    for excluded in exclusions {
        profile.candidates[excluded.0].clear();
    }
    // Remove bindings to excluded options without renumbering other options.
    let sizes: Vec<_> = profile.candidates.iter().map(Vec::len).collect();
    for options in &mut profile.candidates {
        for option in options {
            option.compatible_input_tuples.retain(|tuple| {
                tuple
                    .iter()
                    .all(|binding| binding.price_option < sizes[binding.producer.0])
            });
        }
    }
    Ok(Some((configured.dag.clone(), profile)))
}

/// Restrict only the direct-price proposal. Unsupported configurations and
/// producer interfaces remain available in the full evaluator-refined search.
fn direct_cost_seed(
    configured: &PhysicalCandidatesWithCosts,
    context: &CandidatePlanningContext<'_>,
    seed_exclusions: &[asap_types::stages::PhysicalCandidateId],
    latest_panes: bool,
) -> Result<
    Option<(
        crate::search::candidates::dag::DagOfPhysicalCandidatesWithCosts,
        crate::search::milp::direct_cost::DirectCostProfile,
    )>,
    String,
> {
    use crate::search::costing::direct_profile::{
        extract, extract_latest_panes, DirectProfileError,
    };
    use asap_types::stages::{PhysicalCandidateId, ReaderConstraint};
    let extracted = if latest_panes {
        extract_latest_panes(configured, context)
    } else {
        extract(configured, context)
    };
    let mut extracted = match extracted {
        Ok(Some(extracted)) => extracted,
        Ok(None)
        | Err(DirectProfileError::DeadlineExceeded(_))
        | Err(DirectProfileError::Unsupported(_)) => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if context
        .deadline
        .is_some_and(|deadline| std::time::Instant::now() >= deadline)
    {
        return Ok(None);
    }
    let mut graph = configured.dag.clone();
    let mut supported = extracted.supported;
    // One interchangeable execution-scope copy is sufficient for this seed.
    // Keep all copies in the full graph, where independent input bindings may
    // require them; avoiding their permutations bounds the direct-price solve.
    for constraint in &graph.reader_constraints {
        if let ReaderConstraint::ActivationRequires { candidate, .. } = constraint {
            supported[candidate.0] = false;
        }
    }
    for excluded in seed_exclusions {
        supported[excluded.0] = false;
    }
    for (id, candidate) in graph.candidates.iter_mut().enumerate() {
        if context
            .deadline
            .is_some_and(|deadline| std::time::Instant::now() >= deadline)
        {
            return Ok(None);
        }
        if !supported[id] {
            extracted.profile.nodes[id] = Default::default();
            candidate.local_cost = 0.0;
            candidate.producer_candidates_per_input.clear();
            graph
                .reader_constraints
                .push(ReaderConstraint::AllowedOutputs {
                    candidate: PhysicalCandidateId(id),
                    outputs: Vec::new(),
                });
        } else {
            for input in &mut candidate.producer_candidates_per_input {
                input.candidates.retain(|supplier| {
                    supported[supplier.0] && extracted.suppliers[id].contains(supplier)
                });
            }
        }
    }
    for root in &mut graph.query_roots {
        root.candidates.retain(|candidate| supported[candidate.0]);
        if root.candidates.is_empty() {
            return Ok(None);
        }
    }
    Ok(Some((graph, extracted.profile)))
}

/// Price work and state for proposal ordering without an exactness surcharge.
/// Recomposition and validation still decide the selected graph's accuracy.
pub(in crate::search) fn accuracy_baseline_weights(
    configured: &PhysicalCandidatesWithCosts,
    context: &CandidatePlanningContext<'_>,
) -> Result<Vec<f64>, String> {
    accuracy_baseline(configured, context).map(|(weights, _)| weights)
}

/// Conservative per-state DDS budgets for the feasibility seed. Counting
/// quantile computations keeps ratio operands on composed-budget capacity tiers
/// even when their sign contracts are still symbolic. This is a seed-only restriction,
/// not a proof of query accuracy; the complete catalog and final validator keep
/// mixed exact/approximate plans and all otherwise legal capacities.
fn seed_dds_budgets(
    configured: &PhysicalCandidatesWithCosts,
    context: &CandidatePlanningContext<'_>,
    output_queries: &[usize],
) -> Vec<f64> {
    use asap_accuracy::per_input_target;
    use asap_types::ir::{
        operator::AggIntent, properties::AccuracyMetric, NonASAPOp, OperatorNode,
    };
    let graph = &configured.dag;
    let mut budgets = vec![f64::INFINITY; graph.candidates.len()];
    if context.models.accuracy.may_accept_regardless_of_guarantee() {
        return budgets;
    }
    let query_budgets: Vec<_> = context
        .query_roots
        .iter()
        .zip(context.entries)
        .map(|(root, entry)| {
            let operators = match root {
                QueryRoot::Operator(node) => vec![node],
                QueryRoot::Scalar(expression) => expression.operator_refs(),
            };
            let mut seen = std::collections::HashSet::new();
            let count = operators
                .into_iter()
                .flat_map(OperatorNode::reachable)
                .filter(|node| seen.insert(std::rc::Rc::as_ptr(node)))
                .map(|node| match node.non_asap() {
                    Some(NonASAPOp::Aggregate { measures, .. }) => measures
                        .iter()
                        .filter(|measure| matches!(measure, AggIntent::Quantile { .. }))
                        .count(),
                    _ => 0,
                })
                .sum::<usize>();
            (count > 1)
                .then(|| {
                    per_input_target(
                        &entry.requirements.accuracy,
                        AccuracyMetric::Relative,
                        count,
                    )
                })
                .flatten()
                .map(|target| target.max_error)
        })
        .collect();
    let mut pending = Vec::new();
    for (root, &query) in graph.query_roots.iter().zip(output_queries) {
        if let Some(budget) = query_budgets[query] {
            pending.extend(root.candidates.iter().copied().map(|id| (id, budget)));
        }
    }
    while let Some((id, budget)) = pending.pop() {
        if context
            .deadline
            .is_some_and(|limit| std::time::Instant::now() >= limit)
        {
            break;
        }
        if budget < budgets[id.0] {
            budgets[id.0] = budget;
            pending.extend(
                graph.candidates[id.0]
                    .producer_candidates_per_input
                    .iter()
                    .flat_map(|input| input.candidates.iter().copied())
                    .map(|producer| (producer, budget)),
            );
        }
    }
    budgets
}

fn accuracy_baseline(
    configured: &PhysicalCandidatesWithCosts,
    context: &CandidatePlanningContext<'_>,
) -> Result<(Vec<f64>, Vec<asap_types::stages::PhysicalCandidateId>), String> {
    use asap_types::ir::properties::AccuracyMetric;
    use asap_types::ir::schema::FieldDataType;
    use asap_types::ir::ASAPOp;
    let graph = &configured.dag;
    graph.validate().map_err(|error| error.to_string())?;
    if configured.physical.candidates.len() != graph.candidates.len() {
        return Err("baseline templates must be indexed by candidate ID".into());
    }
    let mut output_queries = Vec::new();
    for (query, root) in context.query_roots.iter().enumerate() {
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
    let confidence_required = context
        .entries
        .iter()
        .any(|entry| entry.requirements.accuracy.failure_probability.is_some());
    let dds_budgets = seed_dds_budgets(configured, context, &output_queries);
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
        .flat_map(|(root, _)| root.candidates.iter().copied())
        .collect();
    while let Some(candidate) = pending.pop() {
        if !rank_requested[candidate.0] {
            rank_requested[candidate.0] = true;
            pending.extend(
                graph.candidates[candidate.0]
                    .producer_candidates_per_input
                    .iter()
                    .flat_map(|input| input.candidates.iter().copied()),
            );
        }
    }
    // Scan-widening projections preserve a logical source schema, but cannot
    // appear below the executor's raw TimeRange boundary. Keep original raw
    // source bindings for the feasibility seed; the full graph still contains
    // the useful projected candidates for other consumers.
    let raw_source_groups: std::collections::HashSet<_> = (0..configured.physical.candidates.len())
        .map(asap_types::stages::PhysicalCandidateId)
        .filter(|&id| {
            configured
                .physical
                .operator_nodes_of_candidates_for_memo_groups
                [configured.physical.candidates[id.0].operator_node.0]
                .is_implementation_root
                && raw_scan_with_shifts(configured.physical.physical_node(id))
        })
        .map(|id| configured.physical.memo_group_of(id))
        .collect();
    // Bound the numerical range; these tiers are intentionally heuristic.
    let preference = (graph.candidates.len() as f64 + 1.0).min(1000.0);
    // A missing certificate means unknown cost, not free computation. Advisory
    // query-time work and retained-state estimates guide proposals only; the
    // candidate graph retains its original certified lower-bound coefficients.
    let estimated_costs = crate::search::costing::proposals::estimate_costs(configured, context);
    let proposal_costs: Vec<_> = graph
        .candidates
        .iter()
        .enumerate()
        .map(|(id, candidate)| {
            candidate
                .local_cost
                .max(estimated_costs[id])
                .max(retained_pane_priority_cost(
                    configured.node(id),
                    &configured.physical.candidates[id],
                    context,
                ))
        })
        .collect();
    // Feasibility seeds compare retained states; cost-only seeds compare query
    // work. Large unrelated pane residency must not make CPU savings smaller
    // than the structural node-count tie-break when accuracy is ignored.
    let mut positive_costs: Vec<_> = proposal_costs
        .iter()
        .enumerate()
        .filter_map(|(id, &cost)| {
            (cost > 0.0
                && if context.models.accuracy.may_accept_regardless_of_guarantee() {
                    configured.physical.candidates[id].timing
                        == asap_types::ir::properties::ExecutionTiming::QueryTime
                } else {
                    matches!(configured.node(id).asap(), Some(ASAPOp::PaneChain { .. }))
                })
            .then_some(cost)
        })
        .collect();
    if positive_costs.is_empty() {
        positive_costs.extend(proposal_costs.iter().copied().filter(|cost| *cost > 0.0));
    }
    positive_costs.sort_by(f64::total_cmp);
    let reference_cost = positive_costs
        .get(positive_costs.len().saturating_sub(1) / 2)
        .copied()
        .unwrap_or(0.0);
    let priorities = (0..configured.physical.candidates.len())
        .map(|id| {
            let node = configured.node(id);
            // Accuracy determines admissibility through composition and validation.
            // It must not add a price surcharge to an approximate implementation.
            let cost_priority = if reference_cost > 0.0 {
                let relative = proposal_costs[id] / reference_cost;
                // Preserve ordering beyond the normal linear range, without
                // letting a very fine unused pane destroy numerical scaling.
                preference
                    * if relative <= 1000.0 {
                        relative
                    } else {
                        // Subtract logs instead of taking the log of a ratio
                        // that can overflow for otherwise finite estimates.
                        1000.0
                            + (proposal_costs[id].ln() - reference_cost.ln() - 1000.0_f64.ln())
                                .max(0.0)
                    }
            } else {
                0.0
            };
            // A strict feasibility seed must not spend its limited attempts on
            // estimators whose error still needs unavailable workload facts.
            // This is seed-only: another producer may repair the contract, so
            // the complete graph retains these candidates and validates them.
            let unresolved_estimator_error =
                matches!(node.asap(), Some(ASAPOp::SummaryEstimate { .. }))
                    && {
                        let root = QueryRoot::Operator(std::rc::Rc::clone(node));
                        crate::bound_root_guarantee(
                            &root,
                            asap_types::workload::bounds::series_read(&root, context.data),
                            context.data,
                            &context.models,
                        ).is_none_or(|guarantee| {
                            guarantee.spec.error.evaluate().is_none()
                                || (confidence_required
                                    && !guarantee.is_exact()
                                    && guarantee.spec.failure_probability.evaluate().is_none())
                        })
                    };
            let contract_risk = !context.models.accuracy.may_accept_regardless_of_guarantee()
                && (unresolved_estimator_error || node.guarantee.as_ref().is_some_and(|guarantee| {
                    !guarantee.is_exact()
                        && guarantee.spec.accuracy_metric == AccuracyMetric::Rank
                        && !rank_requested[id]
                }) || (node.guarantee.is_none()
                    && matches!(node.asap(), Some(ASAPOp::SummaryEstimate { .. }))));
            // Highest-shared-node eligibility depends on chosen consumers.
            // Start with explicit build/pane materialization; optional shared
            // non-summary materializations remain in the complete search.
            let contextual_materialization =
                !context.models.accuracy.may_accept_regardless_of_guarantee()
                    && configured.physical.candidates[id].materialization
                        != asap_types::physical::Materialization::NotMaterialized
                    && !matches!(
                        node.asap(),
                        Some(ASAPOp::SummaryAgg { .. } | ASAPOp::PaneChain { .. })
                    );
            let projected_raw_source = raw_source_groups
                .contains(&configured.physical.memo_group_of(asap_types::stages::PhysicalCandidateId(id)))
                && matches!(
                    node.non_asap(),
                    Some(asap_types::ir::NonASAPOp::Project { .. })
                );
            // Unpriced pane pipelines are not free. Prefer candidates whose
            // local query work can be priced for the initial proposal; the
            // complete search retains every legal pane and its certified bound.
            let unknown_work_priority = if context.models.accuracy.may_accept_regardless_of_guarantee()
                && configured.physical.candidates[id].timing
                    == asap_types::ir::properties::ExecutionTiming::QueryTime
                && matches!(node.asap(), Some(ASAPOp::PaneChain { .. }))
                && proposal_costs[id] == 0.0
            {
                preference * preference
            } else {
                0.0
            };
            let insufficient_composed_budget = node.asap()
                .and_then(ASAPOp::produced_state)
                .is_some_and(|family| matches!(family,
                    FieldDataType::Sketch(kind, _)
                        if matches!(kind.params(), asap_types::ir::schema::SketchParams::DDSketch { alpha }
                            if *alpha > dds_budgets[id])));
            Ok((
                1.0 + cost_priority + unknown_work_priority,
                contract_risk || contextual_materialization || projected_raw_source || insufficient_composed_budget,
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let exclusions = priorities
        .iter()
        .enumerate()
        .filter_map(|(id, (_, excluded))| {
            excluded.then_some(asap_types::stages::PhysicalCandidateId(id))
        })
        .collect();
    Ok((
        priorities.into_iter().map(|(weight, _)| weight).collect(),
        exclusions,
    ))
}

fn raw_scan_with_shifts(node: &asap_types::ir::OperatorNode) -> bool {
    match node.non_asap() {
        Some(asap_types::ir::NonASAPOp::Scan { .. }) => true,
        Some(asap_types::ir::NonASAPOp::TimeShift { child, .. }) => raw_scan_with_shifts(child),
        // Projection after an already formed range is a different, legal case.
        _ => false,
    }
}

// A heuristic memory charge, never a lower bound or an evaluator replacement.
// Group/cardinality and producer choices remain authoritative after selection.
fn retained_pane_priority_cost(
    node: &std::rc::Rc<asap_types::ir::OperatorNode>,
    configuration: &asap_types::stages::PhysicalCandidate,
    context: &CandidatePlanningContext<'_>,
) -> f64 {
    use asap_types::ir::{ASAPOp, NonASAPOp};
    use asap_types::physical::{Materialization, Medium};
    let Materialization::MatAtIngestion(medium) = configuration.materialization else {
        return 0.0;
    };
    let Some(ASAPOp::PaneChain {
        family,
        pane_width_ms,
        pane_count,
        reduction,
        child,
        ..
    }) = node.asap()
    else {
        return 0.0;
    };
    let price = match (&context.models.plan_selection.cost_model, medium) {
        (CostModel::UsageOverTime(prices), Medium::Memory) => prices.cost_per_memory_byte_second,
        (CostModel::UsageOverTime(prices), Medium::Disk) => prices.cost_per_disk_byte_second,
        _ => return 0.0,
    };
    let reachable = asap_types::ir::OperatorNode::reachable(node);
    let sources: std::collections::BTreeSet<_> = reachable
        .iter()
        .filter_map(|node| match node.non_asap() {
            Some(NonASAPOp::Scan { source, .. }) => {
                Some(asap_types::workload::bounds::source_name(source))
            }
            _ => None,
        })
        .collect();
    let groups = match reduction {
        asap_types::ir::operator::Reduction::Reduce(keys)
            if keys.keys().is_empty() && !keys.is_without() =>
        {
            1
        }
        _ => asap_types::workload::bounds::declared_series(&sources, context.data)
            .unwrap_or(1)
            .max(1),
    };
    let rows_per_state = asap_types::workload::bounds::source_rows_per_ms(&sources, context.data)
        .map(|rate| (rate * *pane_width_ms as f64 / groups as f64).ceil() as u64);
    let non_negative = matches!(
        child.non_asap(),
        Some(NonASAPOp::Scan { .. } | NonASAPOp::TimeRange { .. })
    ) && sources
        .iter()
        .all(|source| asap_types::workload::bounds::declared_non_negative(context.data, source));
    let (_, bytes) = crate::summary_shape(
        family,
        crate::StateInputRows {
            rows_per_state,
            non_negative,
        },
    );
    let cost = price * bytes as f64 * groups as f64 * (f64::from(*pane_count) + 1.0);
    if cost.is_finite() {
        cost
    } else {
        0.0
    }
}

pub(in crate::search) enum MaterializationOutcome {
    Implemented(PhysicalWorkloadPlan),
    RejectedAssignment,
}

pub(in crate::search) fn materialize_configured_candidate(
    configured: &PhysicalCandidatesWithCosts,
    context: &CandidatePlanningContext<'_>,
    selected: &SelectedCandidateGraph,
    evaluation_number: usize,
    rejections: &mut CandidateRejectionSummary,
) -> Result<Option<PhysicalWorkloadPlan>, String> {
    match materialize_configured_candidate_with_diagnostics(
        configured,
        context,
        selected,
        evaluation_number,
        rejections,
    )? {
        MaterializationOutcome::Implemented(physical) => Ok(Some(physical)),
        MaterializationOutcome::RejectedAssignment => Ok(None),
    }
}

pub(in crate::search) fn materialize_configured_candidate_with_diagnostics(
    configured: &PhysicalCandidatesWithCosts,
    context: &CandidatePlanningContext<'_>,
    selected: &SelectedCandidateGraph,
    evaluation_number: usize,
    rejections: &mut CandidateRejectionSummary,
) -> Result<MaterializationOutcome, String> {
    // Only a typed infeasible assignment justifies a no-good cut. Missing
    // identity, incompatible contracts and unsupported rewrites stop search.
    configured
        .validate_selected(selected)
        .map_err(|error| PhysicalCandidateError::InvalidOrUnsupported(error).to_string())?;
    let mut realized = match context
        .realizer
        .realize(&configured.physical, &selected.to_selected_candidates())
    {
        Ok(realized) => realized,
        Err(PhysicalCandidateError::RejectedAssignment(reason)) => {
            rejections.record("materialization_assignment".into(), &reason);
            return Ok(MaterializationOutcome::RejectedAssignment);
        }
        Err(error) => return Err(error.to_string()),
    };
    // Stage 2 leaves IDs to its caller. A nonempty, stable proposal label keeps
    // selected-plan references and viewer cost lookups consistent.
    realized.plan.id = format!("joint-plan-{evaluation_number}");
    realized.plan.from_logical = format!("joint-logical-{evaluation_number}");
    Ok(MaterializationOutcome::Implemented(realized.plan))
}

pub(in crate::search) fn reconstruct_configured_candidate(
    configured: &PhysicalCandidatesWithCosts,
    context: &CandidatePlanningContext<'_>,
    selected: &SelectedCandidateGraph,
    evaluation_number: usize,
    rejections: &mut CandidateRejectionSummary,
    rejected_queries: &mut Vec<RejectedQuerySubgraph>,
) -> Result<Option<PhysicalWorkloadPlan>, String> {
    let Some(physical) = materialize_configured_candidate(
        configured,
        context,
        selected,
        evaluation_number,
        rejections,
    )?
    else {
        return Ok(None);
    };
    // Reuse the accuracy evaluator's size-independent proof. Other consumers
    // can change sizes, but cannot repair this certified failure of the same
    // complete query dependency subgraph.
    let targets = crate::accuracy_targets(context.entries);
    let mut output_index = 0;
    for (query, root) in physical.roots.iter().enumerate() {
        let mut outputs = Vec::new();
        match &context.query_roots[query] {
            QueryRoot::Operator(_) => {
                outputs.push(output_index);
                output_index += 1;
            }
            QueryRoot::Scalar(expression) => {
                expression.map_operator_refs(&mut |node| {
                    outputs.push(output_index);
                    output_index += 1;
                    std::rc::Rc::clone(node)
                });
            }
        }
        if let Err(failure) =
            crate::guarantee_check(query, root, &targets[query], context.data, &context.models)
        {
            if failure.size_independent && !outputs.is_empty() {
                rejected_queries.push(RejectedQuerySubgraph::from_query_outputs(
                    selected, &outputs,
                )?);
                let category = serde_json::to_value(failure.cause)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_else(|| format!("{:?}", failure.cause));
                rejections.record(category, &failure.reason);
            }
        }
    }
    if !rejected_queries.is_empty() {
        return Ok(None);
    }
    Ok(Some(physical))
}

#[cfg(test)]
pub(in crate::search) fn evaluate_configured_candidate(
    configured: &PhysicalCandidatesWithCosts,
    context: &CandidatePlanningContext<'_>,
    selected: &SelectedCandidateGraph,
    evaluation_number: usize,
    rejections: &mut CandidateRejectionSummary,
    rejected_queries: &mut Vec<RejectedQuerySubgraph>,
) -> Result<Option<(WholePlanSelection, PhysicalWorkloadPlan)>, String> {
    evaluate_configured_candidate_before(
        configured,
        context,
        selected,
        evaluation_number,
        rejections,
        rejected_queries,
    )
    .map_err(|error| error.to_string())
}

pub(in crate::search) fn evaluate_configured_candidate_before(
    configured: &PhysicalCandidatesWithCosts,
    context: &CandidatePlanningContext<'_>,
    selected: &SelectedCandidateGraph,
    evaluation_number: usize,
    rejections: &mut CandidateRejectionSummary,
    rejected_queries: &mut Vec<RejectedQuerySubgraph>,
) -> Result<Option<(WholePlanSelection, PhysicalWorkloadPlan)>, ErrorSelectingPlan> {
    let Some(physical) = reconstruct_configured_candidate(
        configured,
        context,
        selected,
        evaluation_number,
        rejections,
        rejected_queries,
    )
    .map_err(ErrorSelectingPlan::Solver)?
    else {
        return Ok(None);
    };
    let mut selector = PlanSelector::new(context.entries, context.data, context.models, false)?;
    selector.set_calibration(context.calibration);
    selector.offer_before(&physical, context.deadline)?;
    match selector.finish() {
        Ok(result) => Ok(Some(result)),
        Err(ErrorSelectingPlan::NoValidCandidate(rejected)) => {
            for rejection in rejected {
                let category = serde_json::to_value(rejection.cause)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_else(|| format!("{:?}", rejection.cause));
                rejections.record(category, &rejection.reason);
            }
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::candidates::configurations::enumerate_candidate_configurations;
    use crate::search::candidates::inventory::build_candidate_inventory;
    use crate::search::refinement::search::CandidateSearchStatus;
    use asap_logical_asap_optimization::pass1::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::pass2::memo::candidates::try_populate_pass1;
    use asap_logical_asap_optimization::pass2::memo::shared_configurations::PaneWidthDomain;
    use asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups;
    use asap_logical_asap_optimization::test_support::sharing_rules_and_shared_state_groups;
    use asap_logical_asap_optimization::test_support::{entry_with, lower_promql};
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::schema::{FieldDataType, SketchAlgorithm};
    use asap_types::ir::{ASAPOp, OperatorNode};
    use asap_types::physical::Materialization;
    use asap_types::stages::OriginOfCandidateForMemoGroup;
    use asap_types::workload::{
        DataArrival, Predictability, QueryRecurrence, RepeatedDemand, RepetitionInterval,
        SourceFacts,
    };
    use std::time::Duration;

    fn fixture() -> (
        PhysicalCandidatesWithCosts,
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
        let initial = DagOfLogicalMemoGroups::from_roots(&roots);
        let outputs: Vec<_> = (0..2)
            .map(|q| *initial.query_roots[q].as_operator().unwrap())
            .collect();
        let generated = generate_per_computation_candidates(
            &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(
                &roots.clone(),
            ),
        )
        .unwrap();
        let mut memo = sharing_rules_and_shared_state_groups(
            try_populate_pass1(initial, &generated).unwrap(),
            PaneWidthDomain::Complete,
        )
        .unwrap();
        // Keep the real generated shared KLL readers to bound this evaluator
        // integration test; materialization configurations still remain choices.
        for candidates in &mut memo.candidates {
            let shared: Vec<_> = candidates.iter().filter(|candidate| candidate.origin == OriginOfCandidateForMemoGroup::SharedBuild && OperatorNode::reachable(&candidate.root_node).iter().any(|node|
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
        let mut configured = enumerate_candidate_configurations(
            &source,
            &[0, 1],
            &entries,
            &data,
            &asap_logical_asap_optimization::DeploymentCapabilities::default(),
        )
        .unwrap();
        // Keep this refinement fixture's finite search space focused on summary
        // modes. Non-summary shared materialization has separate physical tests.
        for (id, configuration) in configured.physical.candidates.iter().enumerate() {
            if matches!(
                configuration.materialization,
                Materialization::MatAtQuery(_)
            ) && !matches!(
                configured.node(id).asap(),
                Some(ASAPOp::SummaryAgg { .. } | ASAPOp::PaneChain { .. })
            ) {
                configured.dag.reader_constraints.push(
                    asap_types::stages::ReaderConstraint::AllowedOutputs {
                        candidate: asap_types::stages::PhysicalCandidateId(id),
                        outputs: Vec::new(),
                    },
                );
            }
        }
        (
            configured,
            roots.into_iter().map(|(_, root)| root).collect(),
            entries,
            data,
        )
    }

    /// Restricting a seed removes all query-time scan paths and repairs every
    /// producer option reference without changing the original profile.
    #[test]
    fn maintained_seed_prunes_and_remaps_producer_options() {
        use crate::search::costing::direct_profile::extract_price_options;
        let (configured, roots, entries, data) = fixture();
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            query_roots: &roots,
            realizer: &crate::search::test_realizer(&roots, &entries, &data),
            entries: &entries,
            data: &data,
            models: SelectionModels::builtin(),
            calibration: CalibrationFit::default(),
        };
        let profile = extract_price_options(&configured, &context)
            .unwrap()
            .unwrap();
        let original_count: usize = profile.candidates.iter().map(Vec::len).sum();
        let restricted = maintained_state_profile(&configured, &profile, None).unwrap();
        assert!(restricted
            .candidates
            .iter()
            .any(|options| !options.is_empty()));
        assert!(restricted.candidates.iter().map(Vec::len).sum::<usize>() < original_count);
        for (id, options) in restricted.candidates.iter().enumerate() {
            let scan = matches!(
                configured.node(id).non_asap(),
                Some(asap_types::ir::NonASAPOp::Scan { .. })
            );
            for option in options {
                assert!(!scan || option.profile.ingestion);
                assert!(option.profile.ingestion || !option.profile.materialized);
                assert!(!option.compatible_input_tuples.is_empty());
                for binding in option.compatible_input_tuples.iter().flatten() {
                    assert!(binding.price_option < restricted.candidates[binding.producer.0].len());
                }
            }
        }
        assert_eq!(
            profile.candidates.iter().map(Vec::len).sum::<usize>(),
            original_count
        );
        assert!(
            maintained_state_profile(&configured, &profile, Some(std::time::Instant::now()))
                .is_none()
        );
    }

    // An optional direct-price proposal must not abort planning when its
    // schedule construction budget expires; the ordinary search remains usable.
    #[test]
    fn unsupported_schedule_profiles_leave_both_seed_fallbacks_available() {
        let (configured, roots, mut entries, data) = fixture();
        entries[1].recurrence = QueryRecurrence::Unknown;
        let context = CandidatePlanningContext {
            deadline: Some(std::time::Instant::now()),
            refine_materialization: false,
            query_roots: &roots,
            realizer: &crate::search::test_realizer(&roots, &entries, &data),
            entries: &entries,
            data: &data,
            models: SelectionModels::builtin(),
            calibration: CalibrationFit::default(),
        };
        let mut reasons = Vec::new();
        assert!(joint_cost_seed(&configured, &context, &[], &mut reasons)
            .unwrap()
            .is_none());
        assert!(!reasons.is_empty());
        for latest_panes in [false, true] {
            assert!(direct_cost_seed(&configured, &context, &[], latest_panes)
                .unwrap()
                .is_none());
        }
        // Malformed demand is a real input error, not an unsupported model.
        let mut invalid_entries = entries.clone();
        invalid_entries[1].recurrence =
            QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(0)));
        let invalid_context = CandidatePlanningContext {
            deadline: None,
            entries: &invalid_entries,
            ..context
        };
        assert!(joint_cost_seed(&configured, &invalid_context, &[], &mut reasons).is_err());
        for latest_panes in [false, true] {
            assert!(direct_cost_seed(&configured, &invalid_context, &[], latest_panes).is_err());
        }
    }

    // One logical group can serve a repeated reader through panes and an
    // unpredictable reader locally; only selected pane consumers need eligibility.
    #[test]
    fn nongridded_reader_does_not_veto_another_readers_ingestion_panes() {
        use crate::search::milp::model::select_candidates;
        use asap_types::ir::properties::ExecutionTiming;
        let (_, original, mut entries, mut data) = fixture();
        for facts in data.sources.values_mut() {
            facts.values_non_negative = true;
        }
        let roots = vec![original[0].clone(), original[0].clone()];
        let identified: Vec<_> = roots.iter().cloned().enumerate().collect();
        let initial = DagOfLogicalMemoGroups::from_roots(&identified);
        let output = *initial.query_roots[0].as_operator().unwrap();
        let generated = generate_per_computation_candidates(
            &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(
                &identified,
            ),
        )
        .unwrap();
        let mut memo = try_populate_pass1(initial, &generated).unwrap();
        entries[0].recurrence =
            QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(60_000)));
        entries[1].recurrence = QueryRecurrence::Unknown;
        entries[1].predictability = Predictability::Unknown;
        // This test isolates selected-consumer eligibility, not width search.
        asap_logical_asap_optimization::pass2::memo::window_candidates::add_local_window_composition_in_domain(
            &mut memo,
            &entries,
            asap_logical_asap_optimization::pass2::memo::shared_configurations::PaneWidthDomain::SeedGrids,
        );
        let source = build_candidate_inventory(&memo, &[output, output], |_| Ok(0.0)).unwrap();
        let models = SelectionModels::builtin();
        let mut configured = enumerate_candidate_configurations(
            &source,
            &[0, 1],
            &entries,
            &data,
            &asap_logical_asap_optimization::DeploymentCapabilities::default(),
        )
        .unwrap();
        let has_panes: Vec<_> = configured
            .nodes_for_tests()
            .iter()
            .map(|node| {
                OperatorNode::reachable(node)
                    .iter()
                    .any(|node| matches!(node.asap(), Some(ASAPOp::PaneChain { .. })))
            })
            .collect();
        configured.dag.query_roots[0]
            .candidates
            .retain(|id| has_panes[id.0]);
        configured.dag.query_roots[1]
            .candidates
            .retain(|id| !has_panes[id.0]);
        assert!(!configured.dag.query_roots[0].candidates.is_empty());
        let allowed: Vec<_> = configured
            .nodes_for_tests()
            .iter()
            .enumerate()
            .map(|(id, node)| {
                !matches!(node.asap(), Some(ASAPOp::PaneChain { .. }))
                    || configured.physical.candidates[id].timing == ExecutionTiming::IngestionTime
            })
            .collect();
        for candidate in &mut configured.dag.candidates {
            for input in &mut candidate.producer_candidates_per_input {
                input.candidates.retain(|id| allowed[id.0]);
            }
        }
        let selected = select_candidates(&configured.dag)
            .unwrap()
            .expect("repeated reader still has an ingestion pane plan");
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            query_roots: &roots,
            realizer: &crate::search::test_realizer(&roots, &entries, &data),
            entries: &entries,
            data: &data,
            models,
            calibration: CalibrationFit::default(),
        };
        let evaluated = evaluate_configured_candidate(
            &configured,
            &context,
            &selected,
            1,
            &mut CandidateRejectionSummary::default(),
            &mut Vec::new(),
        )
        .unwrap();
        assert!(evaluated.is_some());
        configured.dag.query_roots[1].candidates = configured.dag.query_roots[0].candidates.clone();
        assert!(
            select_candidates(&configured.dag).unwrap().is_none(),
            "unpredictable reader cannot bind to ingestion-only panes"
        );
    }

    #[test]
    fn ratio_seed_uses_composed_dds_capacities_without_removing_full_candidates() {
        let target = AccuracyTarget::max_error(0.05);
        let root = QueryRoot::Operator(lower_promql(
            "quantile_over_time(0.9, latency[1h]) / quantile_over_time(0.5, latency[1h])",
            target.clone(),
        ));
        let identified = vec![(0, root.clone())];
        let initial = DagOfLogicalMemoGroups::from_roots(&identified);
        let output = *initial.query_roots[0].as_operator().unwrap();
        let generated = generate_per_computation_candidates(
            &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(
                &identified,
            ),
        )
        .unwrap();
        let memo = sharing_rules_and_shared_state_groups(
            try_populate_pass1(initial, &generated).unwrap(),
            PaneWidthDomain::Complete,
        )
        .unwrap();
        let source = build_candidate_inventory(&memo, &[output], |_| Ok(0.0)).unwrap();
        let (_, _, _, mut data) = fixture();
        for facts in data.sources.values_mut() {
            facts.values_non_negative = true;
        }
        let roots = [root];
        let entries = [entry_with(target, QueryRecurrence::Unknown)];
        let models = SelectionModels::builtin();
        let configured = enumerate_candidate_configurations(
            &source,
            &[0],
            &entries,
            &data,
            &asap_logical_asap_optimization::DeploymentCapabilities::default(),
        )
        .unwrap();
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            query_roots: &roots,
            realizer: &crate::search::test_realizer(&roots, &entries, &data),
            entries: &entries,
            data: &data,
            models,
            calibration: CalibrationFit::default(),
        };
        let original_count = configured.dag.candidates.len();
        let (_, excluded) = accuracy_baseline(&configured, &context).unwrap();
        let mut coarse = 0;
        let mut composed = 0;
        for (id, node) in configured.nodes_for_tests().iter().enumerate() {
            let Some(FieldDataType::Sketch(kind, _)) = node.asap().and_then(ASAPOp::produced_state)
            else {
                continue;
            };
            let asap_types::ir::schema::SketchParams::DDSketch { alpha } = kind.params() else {
                continue;
            };
            if *alpha >= 0.05 {
                coarse += 1;
                assert!(excluded.contains(&asap_types::stages::PhysicalCandidateId(id)));
            } else if !excluded.contains(&asap_types::stages::PhysicalCandidateId(id)) {
                composed += 1;
            }
        }
        assert!(
            coarse > 0 && composed > 0,
            "both whole and composed capacities remain in the catalog"
        );
        assert_eq!(configured.dag.candidates.len(), original_count);
        let settings = asap_cost::CostInputs {
            ..Default::default()
        };
        let ignored = CandidatePlanningContext {
            models: models
                .with_plan_selection(&settings)
                .with_accuracy(&asap_accuracy::IgnoreAccuracyGuarantees),
            ..context
        };
        assert!(seed_dds_budgets(&configured, &ignored, &[0])
            .iter()
            .all(|budget| budget.is_infinite()));
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
        let initial = DagOfLogicalMemoGroups::from_roots(&roots);
        let outputs: Vec<_> = (0..roots.len())
            .map(|query| *initial.query_roots[query].as_operator().unwrap())
            .collect();
        let generated = generate_per_computation_candidates(
            &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(
                &roots.clone(),
            ),
        )
        .unwrap();
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
        asap_logical_asap_optimization::pass2::memo::window_candidates::add_local_window_composition(
            &mut memo, &entries,
        );
        let memo = sharing_rules_and_shared_state_groups(memo, PaneWidthDomain::Complete).unwrap();
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
        let models = SelectionModels {
            ..SelectionModels::builtin()
        };
        let configured =
            enumerate_candidate_configurations(&source, &[0, 1], &entries, &data, &deployment)
                .unwrap();
        let roots: Vec<_> = roots.into_iter().map(|(_, root)| root).collect();
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            query_roots: &roots,
            realizer: &crate::search::test_realizer(&roots, &entries, &data),
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
        let mut rejected_queries = Vec::new();
        let mut bad_assignment = None;
        let plain = crate::search::refinement::search::search_candidates_with_baseline(
            &configured.dag,
            limits,
            |selected| {
                bad_assignment = Some(selected.clone());
                Ok(evaluate_configured_candidate(
                    &configured,
                    &context,
                    selected,
                    1,
                    &mut rejected,
                    &mut rejected_queries,
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
        assert!(
            !rejected_queries.is_empty(),
            "composed value/top-k failures must supply certified cuts"
        );
        // A deployment may acquire evidence at another shared summary size.
        // Reuse the original predicate's conservative guard, not a new model.
        struct PotentialEvidence;
        impl asap_accuracy::AccuracyEvidenceProvider for PotentialEvidence {}
        let evidence = PotentialEvidence;
        let evidence_context = CandidatePlanningContext {
            deadline: None,
            models: SelectionModels {
                evidence: &evidence,
                ..models
            },
            ..context
        };
        let mut guarded_cuts = Vec::new();
        let _ = evaluate_configured_candidate(
            &configured,
            &evidence_context,
            bad_assignment.as_ref().unwrap(),
            2,
            &mut CandidateRejectionSummary::default(),
            &mut guarded_cuts,
        )
        .unwrap();
        assert!(
            guarded_cuts.is_empty(),
            "custom evidence cannot justify size-independent cuts"
        );

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

    // A projected raw-source producer would put Project below TimeRange at
    // installation. Projection above a range remains a legal seed candidate.
    #[test]
    fn feasibility_seed_distinguishes_raw_source_and_range_projections() {
        use crate::search::candidates::dag::InputProducerCandidates;
        use asap_types::ir::{NonASAPOp, Operator, ProjectItem, ScalarExpr};
        use asap_types::stages::PhysicalCandidateId;
        let (mut configured, roots, entries, data) = fixture();
        let find = |is_range: bool| {
            (0..configured.physical.candidates.len())
                .map(PhysicalCandidateId)
                .filter(|&id| {
                    configured
                        .physical
                        .operator_nodes_of_candidates_for_memo_groups
                        [configured.physical.candidates[id.0].operator_node.0]
                        .is_implementation_root
                })
                .find(|id| {
                    let node = configured.node(id.0);
                    configured.physical.candidates[id.0].materialization
                        == Materialization::NotMaterialized
                        && matches!(
                            (is_range, node.non_asap()),
                            (false, Some(NonASAPOp::Scan { .. }))
                                | (true, Some(NonASAPOp::TimeRange { .. }))
                        )
                })
                .expect("fixture exposes source and range groups")
        };
        let source = find(false);
        let range = find(true);
        let mut projected = Vec::new();
        for supplier in [source, range] {
            let child = configured.node(supplier.0).clone();
            let template = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
                cols: child
                    .schema
                    .fields
                    .iter()
                    .enumerate()
                    .map(|(column, field)| ProjectItem {
                        alias: Some(field.name.clone()),
                        expr: ScalarExpr::Column(column),
                    })
                    .collect(),
                qualifier: None,
                child,
            }))
            .unwrap();
            let id = configured.push_root_like_for_tests(
                template,
                supplier,
                vec![InputProducerCandidates {
                    candidates: vec![supplier].into(),
                }],
            );
            projected.push(id);
        }
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            query_roots: &roots,
            realizer: &crate::search::test_realizer(&roots, &entries, &data),
            entries: &entries,
            data: &data,
            models: SelectionModels::builtin(),
            calibration: CalibrationFit::default(),
        };
        let (_, excluded) = accuracy_baseline(&configured, &context).unwrap();
        assert!(excluded.contains(&projected[0]));
        assert!(!excluded.contains(&projected[1]));
        assert!(!excluded.contains(&source));
        let ignored = asap_cost::CostInputs {
            ..Default::default()
        };
        let (_, ignored_excluded) = accuracy_baseline(
            &configured,
            &CandidatePlanningContext {
                models: SelectionModels::builtin()
                    .with_plan_selection(&ignored)
                    .with_accuracy(&asap_accuracy::IgnoreAccuracyGuarantees),
                ..context
            },
        )
        .unwrap();
        // Runtime raw-window shape restrictions do not depend on accuracy.
        assert!(ignored_excluded.contains(&projected[0]));
        assert!(!ignored_excluded.contains(&projected[1]));
        assert!(!ignored_excluded.contains(&source));
        assert_eq!(
            configured
                .physical
                .implementation_roots(configured.physical.memo_group_of(source))
                .last(),
            Some(&projected[0]),
            "seed policy does not remove the projected candidate from the inventory"
        );
    }

    // Accuracy policy changes feasibility, not the price of a summary candidate;
    // costs and final evaluation still determine which implementation is kept.
    #[test]
    fn accuracy_policy_does_not_penalize_summary_candidates() {
        let (configured, roots, entries, data) = fixture();
        let settings = asap_cost::CostInputs {
            ..Default::default()
        };
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            query_roots: &roots,
            realizer: &crate::search::test_realizer(&roots, &entries, &data),
            entries: &entries,
            data: &data,
            models: SelectionModels::builtin()
                .with_plan_selection(&settings)
                .with_accuracy(&asap_accuracy::IgnoreAccuracyGuarantees),
            calibration: CalibrationFit::default(),
        };
        let id = configured
            .nodes_for_tests()
            .iter()
            .position(|node| {
                matches!(
                    node.asap(),
                    Some(ASAPOp::SummaryAgg {
                        family: FieldDataType::Sketch(..),
                        ..
                    })
                )
            })
            .expect("fixture includes a summary build");
        let (ignored, excluded) = accuracy_baseline(&configured, &context).unwrap();
        assert!(
            excluded.is_empty(),
            "ignored-policy cost seeds retain all materialization choices"
        );
        let enforced = accuracy_baseline_weights(
            &configured,
            &CandidatePlanningContext {
                models: SelectionModels::builtin(),
                ..context
            },
        )
        .unwrap();
        assert_eq!(
            ignored[id], enforced[id],
            "enforcing accuracy must not add an exactness penalty"
        );
    }

    // A shared state can cost more than two smaller states; proposal priorities
    // must account for certified work without changing the solver's cost model.
    #[test]
    fn accuracy_baseline_accounts_for_state_cost() {
        let (mut configured, roots, entries, data) = fixture();
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            query_roots: &roots,
            realizer: &crate::search::test_realizer(&roots, &entries, &data),
            entries: &entries,
            data: &data,
            models: SelectionModels::builtin(),
            calibration: CalibrationFit::default(),
        };
        let original = accuracy_baseline_weights(&configured, &context).unwrap();
        configured.dag.candidates[0].local_cost = 100.0;
        let priced = accuracy_baseline_weights(&configured, &context).unwrap();
        assert!(priced[0] > original[0] + 2.0);
        assert_eq!(priced[1..], original[1..]);
        assert_eq!(configured.dag.candidates[0].local_cost, 100.0);
    }

    // An irrelevant extremely fine pane must not erase the cost distinction
    // between ordinary state configurations in the feasible-plan heuristic.
    #[test]
    fn baseline_cost_scale_is_not_diluted_by_an_outlier() {
        let (mut configured, roots, entries, data) = fixture();
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            query_roots: &roots,
            realizer: &crate::search::test_realizer(&roots, &entries, &data),
            entries: &entries,
            data: &data,
            models: SelectionModels::builtin(),
            calibration: CalibrationFit::default(),
        };
        let original = accuracy_baseline_weights(&configured, &context).unwrap();
        for (candidate, cost) in configured.dag.candidates.iter_mut().zip([100.0, 1.0, 1e8]) {
            candidate.local_cost = cost;
        }
        let weighted = accuracy_baseline_weights(&configured, &context).unwrap();
        assert!(weighted[0] > original[0] + 2.0);
    }

    // Uncertified memory is not free: a 1200-pane shared state must not beat
    // two much smaller same-family states solely by saving one activation.
    #[test]
    fn baseline_prices_uncertified_pane_storage_without_changing_bounds() {
        use asap_types::ir::{NonASAPOp, Operator};
        use asap_types::physical::Medium;
        let (mut configured, roots, entries, data) = fixture();
        let exact = vec![(
            0usize,
            QueryRoot::Operator(lower_promql(
                "sum_over_time(latency[20m])",
                AccuracyTarget::exact(),
            )),
        )];
        let memo = try_populate_pass1(
            DagOfLogicalMemoGroups::from_roots(&exact),
            &generate_per_computation_candidates(
                &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(
                    &exact,
                ),
            )
            .unwrap(),
        )
        .unwrap();
        let build = memo
            .candidates
            .iter()
            .flatten()
            .flat_map(|candidate| OperatorNode::reachable(&candidate.root_node))
            .find(|node| {
                matches!(
                    node.asap(),
                    Some(ASAPOp::SummaryAgg {
                        family: FieldDataType::ExactAggregate(..),
                        ..
                    })
                )
            })
            .unwrap();
        let Some(ASAPOp::SummaryAgg {
            child,
            family,
            input,
            reduction,
            grouping,
            filter,
        }) = build.asap()
        else {
            unreachable!()
        };
        let child = match child.non_asap() {
            Some(NonASAPOp::TimeRange { child, .. }) => child,
            _ => child,
        };
        for (id, (pane_width_ms, pane_count)) in [(1000, 1200), (1000, 60), (30000, 40)]
            .into_iter()
            .enumerate()
        {
            configured.set_node_for_tests(
                id,
                OperatorNode::new_shared(Operator::ASAP(ASAPOp::PaneChain {
                    child: child.clone(),
                    family: family.clone(),
                    input: input.clone(),
                    reduction: reduction.clone(),
                    grouping: grouping.clone(),
                    filter: filter.clone(),
                    pane_width_ms,
                    pane_count,
                }))
                .unwrap(),
            );
            configured.physical.candidates[id].materialization =
                Materialization::MatAtIngestion(Medium::Memory);
            configured.physical.candidates[id].timing =
                asap_types::ir::properties::ExecutionTiming::IngestionTime;
        }
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            query_roots: &roots,
            realizer: &crate::search::test_realizer(&roots, &entries, &data),
            entries: &entries,
            data: &data,
            models: SelectionModels::builtin(),
            calibration: CalibrationFit::default(),
        };
        let weights = accuracy_baseline_weights(&configured, &context).unwrap();
        assert!(weights[0] > weights[1] + weights[2]);
        assert!(configured
            .dag
            .candidates
            .iter()
            .all(|candidate| candidate.local_cost == 0.0));
    }

    // Explicit rank requirements remain valid baseline priorities, rather than
    // being treated as value-error requests or eliminating rank candidates.
    #[test]
    fn accuracy_baseline_respects_explicit_rank_targets() {
        use asap_types::ir::properties::AccuracyMetric;
        let (configured, roots, mut entries, data) = fixture();
        let weights = |entries: &[QueryWorkloadEntry]| {
            accuracy_baseline(
                &configured,
                &CandidatePlanningContext {
                    deadline: None,
                    refine_materialization: false,
                    query_roots: &roots,
                    realizer: &crate::search::test_realizer(&roots, entries, &data),
                    entries,
                    data: &data,
                    models: SelectionModels::builtin(),
                    calibration: CalibrationFit::default(),
                },
            )
            .unwrap()
        };
        let (value_weights, value_exclusions) = weights(&entries);
        for entry in &mut entries {
            entry.requirements.accuracy.accuracy_metric = Some(AccuracyMetric::Rank);
        }
        let (rank_weights, rank_exclusions) = weights(&entries);
        let rank_nodes: Vec<_> = configured
            .nodes_for_tests()
            .iter()
            .enumerate()
            .filter(|(id, _)| {
                configured
                    .dag
                    .query_roots
                    .iter()
                    .any(|root| root.candidates.iter().any(|candidate| candidate.0 == *id))
            })
            .filter_map(|(id, node)| {
                node.guarantee
                    .as_ref()
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
            .all(|id| value_exclusions.iter().any(|excluded| excluded.0 == *id)));
        assert!(rank_nodes
            .iter()
            .all(|id| rank_exclusions.iter().all(|excluded| excluded.0 != *id)));

        assert!(rank_nodes
            .iter()
            .all(|&id| rank_weights[id] == value_weights[id]));
        assert!(configured
            .dag
            .candidates
            .iter()
            .all(|candidate| candidate.local_cost == 0.0));
    }

    // Both scalar price objectives use the full evaluator, with one frozen calibration.
    #[test]
    fn real_kll_search_cost_matches_independent_plan_selector_evaluation() {
        let (configured, roots, entries, data) = fixture();
        for provisioned in [false, true] {
            let mut settings = asap_cost::CostInputs::default();
            if provisioned {
                let CostModel::UsageOverTime(prices) = settings.cost_model.clone() else {
                    unreachable!()
                };
                settings.cost_model = CostModel::ProvisionedCapacity(prices);
            }
            let models = SelectionModels {
                plan_selection: &settings,
                ..SelectionModels::builtin()
            };
            let context = CandidatePlanningContext {
                deadline: None,
                refine_materialization: false,
                query_roots: &roots,
                realizer: &crate::search::test_realizer(&roots, &entries, &data),
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
            deadline: None,
            refine_materialization: false,
            query_roots: &roots,
            realizer: &crate::search::test_realizer(&roots, &entries, &data),
            entries: &entries,
            data: &data,
            models: SelectionModels::builtin(),
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
            deadline: None,
            refine_materialization: false,
            query_roots: &roots,
            realizer: &crate::search::test_realizer(&roots, &entries, &data),
            entries: &entries,
            data: &data,
            models: SelectionModels::builtin(),
            calibration: CalibrationFit::default(),
        };
        // Select an ingestion-only summary assignment explicitly. Solver
        // traversal order can change with warm starts and must not determine
        // whether this test exercises authoritative eligibility rejection.
        let mut ingestion_graph = configured.dag.clone();
        for (id, configuration) in configured.physical.candidates.iter().enumerate() {
            if matches!(
                configured.node(id).asap(),
                Some(ASAPOp::SummaryAgg { .. } | ASAPOp::PaneChain { .. })
            ) && !matches!(
                configuration.materialization,
                Materialization::MatAtIngestion(_)
            ) {
                ingestion_graph.reader_constraints.push(
                    asap_types::stages::ReaderConstraint::AllowedOutputs {
                        candidate: asap_types::stages::PhysicalCandidateId(id),
                        outputs: vec![],
                    },
                );
            }
        }
        let rejected =
            crate::search::milp::model::solve_candidate_proposal(&ingestion_graph, &[], None)
                .unwrap()
                .unwrap()
                .selected;
        let mut rejections = CandidateRejectionSummary::default();
        assert!(evaluate_configured_candidate(
            &configured,
            &context,
            &rejected,
            0,
            &mut rejections,
            &mut Vec::new()
        )
        .unwrap()
        .is_none());
        assert_eq!(
            rejections.counts.get("materialization_assignment"),
            Some(&1)
        );
        assert!(!rejections.examples.is_empty());
        let result = optimize_configured_candidates(
            &configured,
            &context,
            CandidateSearchLimits {
                max_refinements: 20,
                wall_time: Duration::from_secs(20),
            },
        )
        .unwrap();
        // The bounded search must recover a legal plan; proving optimality is
        // tested separately and depends on the generated catalog's size.
        assert!(matches!(
            result.search.status,
            CandidateSearchStatus::Optimal | CandidateSearchStatus::RefinementLimit
        ));
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
            deadline: None,
            refine_materialization: false,
            query_roots: &roots,
            realizer: &crate::search::test_realizer(&roots, &entries, &data),
            entries: &entries,
            data: &data,
            models: SelectionModels::builtin(),
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
    fn scalar_price_search_does_not_scalarize_cluster_objectives() {
        let (configured, roots, entries, data) = fixture();
        let settings = asap_cost::CostInputs {
            cost_model: CostModel::ClusterCapacity(asap_cost::ClusterLimits {
                memory_bytes: u64::MAX,
                disk_bytes: u64::MAX,
                cpu_cores: None,
                objective: asap_cost::ClusterObjective::LowestLatency,
            }),
            ..Default::default()
        };
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            query_roots: &roots,
            realizer: &crate::search::test_realizer(&roots, &entries, &data),
            entries: &entries,
            data: &data,
            models: SelectionModels {
                plan_selection: &settings,
                ..SelectionModels::builtin()
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
