//! Advisory local costs for proposal ordering when a universal bound is unknown.
//! Price original template inputs, not every possible selected connection. These
//! estimates must never become certified bounds or feasibility constraints.
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use asap_types::ir::physical_export::PhysicalASAPDAGNode;
use asap_types::ir::properties::execution_data_state::{DataPrimitive, ExecutionDataState};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::{ASAPOp, NonASAPOp, OperatorNode};
use asap_types::physical::Materialization;

use crate::search::candidates::configurations::CandidateConfigurationInventory;
use crate::search::refinement::planner::CandidatePlanningContext;
use crate::{price_node, PricedNode, ProducerOutput};

/// Partial heuristic estimates are harmless: unknown or interrupted entries
/// remain zero, and the surrounding search still enforces its shared deadline.
pub(in crate::search) fn estimate_costs(
    configured: &CandidateConfigurationInventory,
    context: &CandidatePlanningContext<'_>,
) -> Vec<f64> {
    let mut estimates = vec![0.0; configured.inventory.graph.candidates.len()];
    let mut templates = TemplateCatalog::default();
    let mut roots = vec![None; estimates.len()];
    for (id, configuration) in configured.configurations.iter().enumerate() {
        if expired(context.deadline) {
            break;
        }
        if configuration.timing != ExecutionTiming::QueryTime
            || configured.inventory.graph.candidates[id].local_cost > 0.0
        {
            continue;
        }
        if let Some(node) = &configured.inventory.templates[id] {
            roots[id] = templates.append(node, None, context.deadline);
        }
    }
    // A singleton request rate gives a consistent initial priority. Actual
    // selected-reader unions, retained lifetimes and shared resources are priced
    // only by the complete evaluator after connections have been selected.
    let rate = context
        .entries
        .iter()
        .filter_map(|entry| {
            crate::cost::execution_demand::evaluation_rate(
                [(&entry.recurrence, entry.range_evaluation)],
                context
                    .models
                    .plan_selection
                    .non_repeating_query_horizon_seconds,
            )
            .ok()
        })
        .reduce(f64::min)
        .unwrap_or(0.0);
    let mut prices: Vec<Option<PricedNode<'_>>> = Vec::with_capacity(templates.nodes.len());
    for (id, node) in templates.nodes.iter().enumerate() {
        if expired(context.deadline) {
            break;
        }
        let producers: Option<Vec<_>> = templates.children[id]
            .iter()
            .map(|&child| {
                let priced = prices[child].as_ref()?;
                Some(ProducerOutput {
                    payload: &templates.nodes[child].payload,
                    output_schema: &templates.nodes[child].output_schema,
                    output: priced.output,
                    sources: priced.sources.clone(),
                    family: priced.family,
                    unchanged_columns: priced.unchanged_columns.clone(),
                })
            })
            .collect();
        prices.push(producers.and_then(|producers| {
            price_node(
                node,
                &producers,
                (1, rate),
                crate::trailing_window(templates.lookbacks[id]),
                1,
                None,
                context.data,
                context.models.plan_selection,
                context.calibration.factor(),
            )
            .ok()
        }));
    }
    for (estimate, root) in estimates.iter_mut().zip(roots) {
        if let Some(cost) = root.and_then(|id| prices.get(id)?.as_ref()?.cost.cost_per_second) {
            if cost.is_finite() && cost >= 0.0 {
                *estimate = cost;
            }
        }
    }
    estimates
}

fn expired(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

#[derive(Default)]
struct TemplateCatalog {
    nodes: Vec<PhysicalASAPDAGNode>,
    children: Vec<Vec<usize>>,
    lookbacks: Vec<Option<u64>>,
    ids: HashMap<(*const OperatorNode, Option<u64>), usize>,
}
impl TemplateCatalog {
    fn append(
        &mut self,
        node: &Rc<OperatorNode>,
        lookback: Option<u64>,
        deadline: Option<Instant>,
    ) -> Option<usize> {
        if expired(deadline) {
            return None;
        }
        let lookback = match node.non_asap() {
            Some(NonASAPOp::TimeRange { range, .. }) => {
                Some(u64::try_from(range.as_millis()).ok()?)
            }
            _ => lookback,
        };
        let lookback = match node.asap() {
            // Query-time construction reads all panes' source rows. Reuse the
            // compact PaneChain formula in price_node without expanding panes.
            Some(ASAPOp::PaneChain {
                pane_width_ms,
                pane_count,
                ..
            }) => Some(pane_width_ms.checked_mul(u64::from(*pane_count))?),
            _ => lookback,
        };
        let key = (Rc::as_ptr(node), lookback);
        if let Some(&id) = self.ids.get(&key) {
            return Some(id);
        }
        let mut children = Vec::new();
        let mut complete = true;
        let payload = node.operator.map_children(|child| {
            let position = children.len();
            match self.append(child, lookback, deadline) {
                Some(child) => children.push(child),
                None => complete = false,
            }
            position
        });
        if !complete {
            return None;
        }
        let id = self.nodes.len();
        self.nodes.push(PhysicalASAPDAGNode {
            id,
            payload,
            output_state: ExecutionDataState {
                timing: ExecutionTiming::QueryTime,
                primitive: if node.asap().and_then(ASAPOp::produced_state).is_some() {
                    DataPrimitive::SummaryState
                } else {
                    DataPrimitive::Raw
                },
            },
            materialization: Materialization::NotMaterialized,
            output_schema: node.schema.clone(),
            guarantee: node.guarantee.clone(),
            retention_ms: None,
        });
        self.children.push(children);
        self.lookbacks.push(lookback);
        self.ids.insert(key, id);
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::candidates::configurations::enumerate_candidate_configurations;
    use crate::search::candidates::inventory::build_candidate_inventory;
    use crate::{CalibrationFit, PlanningModels};
    use asap_logical_asap_optimization::memo::candidates::try_populate_pass1;
    use asap_logical_asap_optimization::memo::{LogicalMemo, MemoGroupId};
    use asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::test_support::{entry_with, lower_promql};
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::schema::{FieldDataType, SketchAlgorithm};
    use asap_types::ir::QueryRoot;
    use asap_types::workload::*;

    // Use the production analytical model to distinguish a whole-window exact
    // quantile from KLL construction, without turning either estimate into a bound.
    #[test]
    fn query_time_estimates_distinguish_exact_sort_and_kll_update_cost() {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let roots = vec![QueryRoot::Operator(lower_promql(
            "quantile_over_time(0.9, data[5m])",
            target.clone(),
        ))];
        let identified = roots.iter().cloned().enumerate().collect::<Vec<_>>();
        let initial = LogicalMemo::from_roots(&identified);
        let output = MemoGroupId(
            initial
                .groups
                .iter()
                .position(|group| group.roots_of.contains(&0))
                .unwrap() as u32,
        );
        let memo = try_populate_pass1(
            initial,
            &generate_per_computation_candidates(identified).unwrap(),
        )
        .unwrap();
        let inventory = build_candidate_inventory(&memo, &[output], |_| Ok(0.0)).unwrap();
        // One canonical scan can supply different windows. Cache the priced
        // context separately while reusing a repeated identical request.
        let scan = inventory
            .templates
            .iter()
            .flatten()
            .find(|node| matches!(node.non_asap(), Some(NonASAPOp::Scan { .. })))
            .unwrap();
        let mut contexts = TemplateCatalog::default();
        let short = contexts.append(scan, Some(60_000), None).unwrap();
        assert_eq!(short, contexts.append(scan, Some(60_000), None).unwrap());
        assert_ne!(short, contexts.append(scan, Some(300_000), None).unwrap());

        let entries = vec![entry_with(
            target,
            QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(10000))),
        )];
        let data = DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 666.67,
            },
            sources: [(
                "data".into(),
                SourceFacts {
                    series_count: Some(10000),
                    ..Default::default()
                },
            )]
            .into_iter()
            .collect(),
        };
        let models = PlanningModels::builtin();
        let mut configured = enumerate_candidate_configurations(
            &inventory,
            &[0],
            &entries,
            &data,
            models.deployment_capabilities,
        )
        .unwrap();
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
        let estimates = estimate_costs(&configured, &context);
        let mut exact = Vec::new();
        let mut kll = Vec::new();
        for (id, template) in configured.inventory.templates.iter().enumerate() {
            if configured.configurations[id].timing != ExecutionTiming::QueryTime {
                continue;
            }
            match template.as_ref().unwrap().asap() {
                Some(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(kind, _),
                    ..
                }) if *kind.algorithm() == SketchAlgorithm::Kll => kll.push(estimates[id]),
                _ if matches!(
                    template.as_ref().unwrap().non_asap(),
                    Some(NonASAPOp::Aggregate { .. })
                ) =>
                {
                    exact.push(estimates[id])
                }
                _ => {}
            }
        }
        assert!(!exact.is_empty() && !kll.is_empty());
        assert!(exact.iter().chain(&kll).all(|cost| *cost > 0.0));
        assert!(
            kll.iter().copied().fold(f64::INFINITY, f64::min)
                < exact.iter().copied().fold(f64::INFINITY, f64::min)
        );
        assert!(configured
            .inventory
            .graph
            .candidates
            .iter()
            .all(|candidate| candidate.local_cost == 0.0));
        // A query-time pane chain reads its full window, not an unbounded
        // source. Price it compactly; a missing horizon previously made every
        // pane estimate zero and wrongly favored it over priced whole builds.
        let pane_id = configured
            .inventory
            .templates
            .iter()
            .enumerate()
            .find_map(|(id, node)| {
                (configured.configurations[id].timing == ExecutionTiming::QueryTime
                    && matches!(
                        node.as_ref().unwrap().asap(),
                        Some(ASAPOp::SummaryAgg { .. })
                    ))
                .then_some(id)
            })
            .unwrap();
        let build = configured.inventory.templates[pane_id].as_ref().unwrap();
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
        let Some(NonASAPOp::TimeRange { child: source, .. }) = child.non_asap() else {
            unreachable!()
        };
        let pane = OperatorNode::new_shared(asap_types::ir::Operator::ASAP(ASAPOp::PaneChain {
            child: Rc::clone(source),
            family: family.clone(),
            input: input.clone(),
            reduction: reduction.clone(),
            grouping: grouping.clone(),
            filter: filter.clone(),
            pane_width_ms: 1000,
            pane_count: 300,
        }))
        .unwrap();
        configured.inventory.templates[pane_id] = Some(pane);
        let pane_estimates = estimate_costs(&configured, &context);
        assert!(
            pane_estimates[pane_id] > 0.0,
            "query-time pane build must not look free"
        );
        assert_eq!(
            configured.inventory.graph.candidates[pane_id].local_cost,
            0.0
        );

        // Deadline cancellation merely omits advisory priorities; no inventory
        // or feasibility fact is changed by incomplete heuristic work.
        let expired = CandidatePlanningContext {
            deadline: Some(Instant::now()),
            ..context
        };
        assert!(estimate_costs(&configured, &expired)
            .iter()
            .all(|cost| *cost == 0.0));
    }
}
