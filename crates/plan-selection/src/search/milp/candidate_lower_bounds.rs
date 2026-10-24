//! Certified node-local usage bounds for fixed DDS/exact state and its query readers.
//! Consumer-dependent held memory, raw retention, request fees and extra copies
//! remain in the complete evaluator. Unsupported pricing contexts contribute zero.
use asap_types::ir::physical_export::PhysicalASAPDAGNode;
use asap_types::ir::properties::execution_data_state::{DataPrimitive, ExecutionDataState};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::schema::{FieldDataType, SketchParams};
use asap_types::ir::{ASAPOp, NonASAPOp};
use asap_types::physical::Materialization;

use super::candidate_configurations::CandidateConfigurationInventory;
use super::candidate_planner::CandidatePlanningContext;
use crate::{price_node, CostModel, PricedNode, ProducerOutput, StateFamily};

#[derive(Debug, Default, Clone, Copy)]
pub(super) struct LowerBoundMetrics {
    pub priced_candidates: usize,
    pub ingestion_scans: usize,
    pub ingestion_pane_chains: usize,
    pub query_readers: usize,
    pub ingestion_transforms: usize,
}

/// Replace placeholder costs in the actual inventory used by selection and
/// reconstruction. This is a lower bound, never a substitute for plan evaluation.
pub(super) fn apply(
    configured: &mut CandidateConfigurationInventory,
    context: &CandidatePlanningContext<'_>,
) -> Result<LowerBoundMetrics, String> {
    configured
        .inventory
        .graph
        .validate()
        .map_err(|e| e.to_string())?;
    let count = configured.inventory.graph.candidates.len();
    if configured.configurations.len() != count || configured.inventory.templates.len() != count {
        return Err("lower-bound inputs must be indexed by candidate ID".into());
    }
    let mut metrics = LowerBoundMetrics::default();
    let mut costs = vec![0.0; count];
    if matches!(
        context.models.plan_selection.cost_model,
        CostModel::UsageOverTime(_)
    ) {
        // A selected query-time node must serve at least one query boundary.
        // Removing range expansion also covers ancestors of retained state;
        // taking the minimum singleton rate permits every possible reader set.
        let query_rate_floor =
            context
                .entries
                .iter()
                .try_fold(f64::INFINITY, |minimum, entry| {
                    crate::cost::execution_demand::evaluation_rate(
                        [(&entry.recurrence, None)],
                        context
                            .models
                            .plan_selection
                            .non_repeating_query_horizon_seconds,
                    )
                    .map(|rate| minimum.min(rate))
                    .map_err(|error| error.to_string())
                })?;
        let query_rate_floor = if query_rate_floor.is_finite() {
            query_rate_floor
        } else {
            0.0
        };
        // Retained-state refresh policies can evolve independently of request
        // accounting. Do not certify query work underneath any possible retained
        // consumer, even when the current policy would permit this floor.
        let mut retained_ancestor = vec![false; count];
        let mut pending: Vec<_> = configured
            .configurations
            .iter()
            .enumerate()
            .filter_map(|(id, config)| {
                matches!(
                    config.materialization,
                    Materialization::MatAtQueryKeptAcrossEvaluations(_)
                )
                .then_some(id)
            })
            .collect();
        while let Some(id) = pending.pop() {
            if std::mem::replace(&mut retained_ancestor[id], true) {
                continue;
            }
            pending.extend(
                configured.inventory.graph.candidates[id]
                    .producer_requirements
                    .iter()
                    .flat_map(|input| input.compatible_producers.iter().map(|producer| producer.0)),
            );
        }
        let nodes: Vec<_> = (0..count)
            .map(|id| {
                let configuration = configured.configurations[id];
                let template = configured.inventory.templates[id].as_ref()?;
                let state = if configuration.timing == ExecutionTiming::QueryTime {
                    if retained_ancestor[id] {
                        return None;
                    }
                    if !matches!(
                        configuration.materialization,
                        Materialization::NotMaterialized | Materialization::MatAtQuery(_)
                    ) {
                        return None;
                    }
                    match (template.non_asap(), template.asap()) {
                        (Some(NonASAPOp::Project { .. }), _)
                        | (
                            _,
                            Some(
                                ASAPOp::SummaryEstimate { .. }
                                | ASAPOp::FinalizeExactAccumulator { .. },
                            ),
                        ) => ExecutionDataState::QUERY_ROWS,
                        (_, Some(ASAPOp::SummaryMerge { .. })) => ExecutionDataState {
                            timing: ExecutionTiming::QueryTime,
                            primitive: DataPrimitive::SummaryState,
                        },
                        _ => return None,
                    }
                } else {
                    match (
                        template.non_asap(),
                        template.asap(),
                        configuration.materialization,
                    ) {
                        (
                            Some(
                                NonASAPOp::Scan { .. }
                                | NonASAPOp::Project { .. }
                                | NonASAPOp::TimeRange { .. },
                            ),
                            _,
                            Materialization::NotMaterialized,
                        ) => ExecutionDataState::INGESTION_ROWS,
                        (
                            _,
                            Some(ASAPOp::PaneChain {
                                family: FieldDataType::ExactAggregate(..),
                                ..
                            }),
                            Materialization::MatAtIngestion(_),
                        ) => ExecutionDataState::INGESTION_SUMMARY,
                        (
                            _,
                            Some(ASAPOp::PaneChain {
                                family: FieldDataType::Sketch(kind, _),
                                ..
                            }),
                            Materialization::MatAtIngestion(_),
                        ) if matches!(kind.params(), SketchParams::DDSketch { .. }) => {
                            ExecutionDataState::INGESTION_SUMMARY
                        }
                        _ => return None,
                    }
                };
                let mut position = 0;
                let payload = template.operator.map_children(|_| {
                    let id = position;
                    position += 1;
                    id
                });
                if position
                    != configured.inventory.graph.candidates[id]
                        .producer_requirements
                        .len()
                {
                    return None;
                }
                Some(PhysicalASAPDAGNode {
                    id,
                    payload,
                    output_state: state,
                    materialization: configuration.materialization,
                    output_schema: template.schema.clone(),
                    guarantee: template.guarantee.clone(),
                })
            })
            .collect();
        let mut cache = vec![None; count];
        let mut visiting = vec![false; count];
        let mut complete = vec![false; count];
        for id in 0..count {
            if let Some(priced) = certified_price(
                id,
                configured,
                &nodes,
                context,
                query_rate_floor,
                &mut cache,
                &mut visiting,
                &mut complete,
            )? {
                let cost = priced.cost.cost_per_second.unwrap_or(0.0);
                if cost.is_finite() && cost > 0.0 {
                    costs[id] = cost;
                    metrics.priced_candidates += 1;
                    if nodes[id].as_ref().unwrap().output_state.timing == ExecutionTiming::QueryTime
                    {
                        metrics.query_readers += 1;
                    } else {
                        match &nodes[id].as_ref().unwrap().payload {
                            asap_types::ir::Operator::NonASAP(NonASAPOp::Scan { .. }) => {
                                metrics.ingestion_scans += 1
                            }
                            asap_types::ir::Operator::ASAP(ASAPOp::PaneChain { .. }) => {
                                metrics.ingestion_pane_chains += 1
                            }
                            _ => metrics.ingestion_transforms += 1,
                        }
                    }
                }
            }
        }
    }
    for (candidate, cost) in configured.inventory.graph.candidates.iter_mut().zip(costs) {
        candidate.local_cost = cost;
    }
    configured
        .inventory
        .graph
        .validate()
        .map_err(|e| e.to_string())?;
    Ok(metrics)
}

#[allow(clippy::too_many_arguments)]
fn certified_price<'a>(
    id: usize,
    configured: &CandidateConfigurationInventory,
    nodes: &'a [Option<PhysicalASAPDAGNode>],
    context: &'a CandidatePlanningContext<'a>,
    query_rate_floor: f64,
    cache: &mut [Option<PricedNode<'a>>],
    visiting: &mut [bool],
    complete: &mut [bool],
) -> Result<Option<PricedNode<'a>>, String> {
    if complete[id] {
        return Ok(cache[id].clone());
    }
    if visiting[id] {
        return Ok(None);
    }
    let Some(node) = nodes[id].as_ref() else {
        complete[id] = true;
        return Ok(None);
    };
    visiting[id] = true;
    let result: Result<Option<PricedNode<'a>>, String> = (|| {
        let mut views = Vec::new();
        for requirement in &configured.inventory.graph.candidates[id].producer_requirements {
            let mut representative = None;
            let mut signature = None;
            for supplier in &requirement.compatible_producers {
                let Some(priced) = certified_price(
                    supplier.0,
                    configured,
                    nodes,
                    context,
                    query_rate_floor,
                    cache,
                    visiting,
                    complete,
                )?
                else {
                    return Ok(None);
                };
                let producer = nodes[supplier.0].as_ref().unwrap();
                // For the certified operator whitelist, price_node observes
                // a supplier's raw-sample classification, pane count, family,
                // schema, statistics, sources and unchanged columns. Different
                // projection expressions with those same outputs have identical
                // consumer pricing; their own work is priced separately.
                let raw_samples = matches!(
                    producer.payload,
                    asap_types::ir::Operator::NonASAP(
                        NonASAPOp::Scan { .. } | NonASAPOp::TimeRange { .. }
                    )
                );
                let panes = match producer.payload {
                    asap_types::ir::Operator::ASAP(ASAPOp::PaneChain { pane_count, .. }) => {
                        Some(pane_count)
                    }
                    _ => None,
                };
                let family = match priced.family {
                    StateFamily::None => serde_json::Value::Null,
                    StateFamily::Sketch(params) => serde_json::json!({"sketch": params}),
                    StateFamily::Exact(kind) => serde_json::json!({"exact": kind}),
                };
                let current = serde_json::to_string(&(
                    raw_samples,
                    panes,
                    family,
                    &producer.output_schema,
                    priced.output,
                    &priced.sources,
                    &priced.unchanged_columns,
                ))
                .map_err(|error| error.to_string())?;
                if signature
                    .as_ref()
                    .is_some_and(|previous| previous != &current)
                {
                    return Ok(None);
                }
                signature = Some(current);
                representative = Some(ProducerOutput {
                    payload: &producer.payload,
                    output_schema: &producer.output_schema,
                    output: priced.output,
                    sources: priced.sources,
                    family: priced.family,
                    unchanged_columns: priced.unchanged_columns,
                });
            }
            let Some(view) = representative else {
                return Ok(None);
            };
            views.push(view);
        }
        let retained = match &node.payload {
            asap_types::ir::Operator::ASAP(ASAPOp::PaneChain { pane_count, .. }) => {
                u64::from(*pane_count).max(1)
            }
            _ => 1,
        };
        // One ingestion boundary executes once per accounted second; additional
        // unmaterialized consumers can only add executions. DDSketch and exact
        // accumulators are not resized by failure-probability composition. Retention above its own
        // pane count and all whole-plan charges are deliberately omitted.
        Ok(price_node(
            node,
            &views,
            (
                1,
                if node.output_state.timing == ExecutionTiming::IngestionTime {
                    1.0
                } else {
                    query_rate_floor
                },
            ),
            None,
            retained,
            None,
            context.data,
            context.models.plan_selection,
            context.calibration.factor(),
        )
        .ok())
    })();
    visiting[id] = false;
    complete[id] = true;
    cache[id] = result?;
    Ok(cache[id].clone())
}

#[cfg(test)]
mod tests {
    use super::super::candidate_configurations::CandidateConfiguration;
    use super::super::candidate_connections::{
        solve_candidate_proposal, CandidateDependencyGraph, ObservedAssignment, PhysicalCandidate,
        PhysicalCandidateId, ProducerRequirement, QueryRootCandidates,
    };
    use super::super::candidate_inventory::CandidateInventory;
    use super::super::candidate_physical::materialize_selected_candidates;
    use super::*;
    use crate::{CalibrationFit, PlanSelector, PlanningModels};
    use asap_logical_asap_optimization::asap_aware_cse::{
        pane_candidates, window_composition_pass2_candidates,
    };
    use asap_logical_asap_optimization::memo::candidates::try_populate_pass1;
    use asap_logical_asap_optimization::memo::{LogicalMemo, MemoGroupId};
    use asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::test_support::{entry_with, lower_promql};
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::{OperatorNode, QueryRoot};
    use asap_types::physical::Medium;
    use asap_types::workload::*;
    use std::{collections::HashMap, rc::Rc};

    fn fixture() -> (
        CandidateConfigurationInventory,
        Vec<QueryRoot>,
        Vec<QueryWorkloadEntry>,
        DataWorkload,
    ) {
        fixture_for(false)
    }

    fn fixture_for(
        exact: bool,
    ) -> (
        CandidateConfigurationInventory,
        Vec<QueryRoot>,
        Vec<QueryWorkloadEntry>,
        DataWorkload,
    ) {
        let target = if exact {
            AccuracyTarget::exact()
        } else {
            AccuracyTarget::max_error(0.1).failure_probability(0.1)
        };
        let mut roots = Vec::new();
        for q in [0.95, 0.99] {
            let query = if exact {
                "sum_over_time(latency[5m])".to_string()
            } else {
                format!("quantile_over_time({q}, latency[5m])")
            };
            let root = QueryRoot::Operator(lower_promql(&query, target.clone()));
            let roots_with_ids = vec![(0usize, root)];
            let memo = try_populate_pass1(
                LogicalMemo::from_roots(&roots_with_ids),
                &generate_per_computation_candidates(roots_with_ids).unwrap(),
            )
            .unwrap();
            let root_group = memo
                .groups
                .iter()
                .position(|group| group.roots_of.contains(&0))
                .unwrap();
            let candidate = memo.candidates[root_group].iter().find(|candidate|
                OperatorNode::reachable(&candidate.node).iter().any(|node| if exact {
                    matches!(node.asap(), Some(ASAPOp::SummaryAgg { family: FieldDataType::ExactAggregate(..), .. }))
                } else { matches!(node.asap(), Some(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. }) if matches!(kind.params(), SketchParams::DDSketch { .. })) })
            ).unwrap();
            let input = vec![QueryRoot::Operator(Rc::clone(&candidate.node))];
            let panes = pane_candidates(&input, &[Some(60_000)]);
            roots.push(
                window_composition_pass2_candidates(&input, &panes)
                    .unwrap()
                    .remove(0)
                    .roots
                    .unwrap()
                    .remove(0),
            );
        }
        let mut configured = CandidateConfigurationInventory {
            inventory: CandidateInventory {
                graph: CandidateDependencyGraph::default(),
                templates: Vec::new(),
                group_candidates: Vec::new(),
            },
            configurations: Vec::new(),
        };
        fn intern(
            node: &Rc<OperatorNode>,
            configured: &mut CandidateConfigurationInventory,
            ids: &mut HashMap<*const OperatorNode, usize>,
        ) -> usize {
            if let Some(&id) = ids.get(&Rc::as_ptr(node)) {
                return id;
            }
            let mut inputs = Vec::new();
            node.operator.map_children(|child| {
                inputs.push(intern(child, configured, ids));
                Rc::clone(child)
            });
            let id = configured.inventory.templates.len();
            ids.insert(Rc::as_ptr(node), id);
            let pane = matches!(node.asap(), Some(ASAPOp::PaneChain { .. }));
            let scan = matches!(node.non_asap(), Some(NonASAPOp::Scan { .. }));
            configured.configurations.push(CandidateConfiguration {
                source_candidate: PhysicalCandidateId(id),
                timing: if pane || scan {
                    ExecutionTiming::IngestionTime
                } else {
                    ExecutionTiming::QueryTime
                },
                materialization: if pane {
                    Materialization::MatAtIngestion(Medium::Memory)
                } else {
                    Materialization::NotMaterialized
                },
            });
            configured.inventory.templates.push(Some(Rc::clone(node)));
            configured
                .inventory
                .graph
                .candidates
                .push(PhysicalCandidate {
                    group: MemoGroupId(id as u32),
                    local_cost: 0.0,
                    producer_requirements: inputs
                        .into_iter()
                        .map(|input| ProducerRequirement {
                            compatible_producers: vec![PhysicalCandidateId(input)],
                        })
                        .collect(),
                });
            id
        }
        let mut ids = HashMap::new();
        for root in &roots {
            let QueryRoot::Operator(node) = root else {
                unreachable!()
            };
            let id = intern(node, &mut configured, &mut ids);
            configured
                .inventory
                .graph
                .query_roots
                .push(QueryRootCandidates {
                    compatible_candidates: vec![PhysicalCandidateId(id)],
                });
        }
        let chains: Vec<_> = configured
            .inventory
            .templates
            .iter()
            .enumerate()
            .filter_map(|(id, node)| {
                matches!(
                    node.as_ref().unwrap().asap(),
                    Some(ASAPOp::PaneChain { .. })
                )
                .then_some(PhysicalCandidateId(id))
            })
            .collect();
        for candidate in &mut configured.inventory.graph.candidates {
            for input in &mut candidate.producer_requirements {
                if chains.contains(&input.compatible_producers[0]) {
                    input.compatible_producers = chains.clone();
                }
            }
        }
        let entries = (0..2)
            .map(|_| {
                let mut entry = entry_with(
                    target.clone(),
                    QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
                        60_000,
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
                    series_count: Some(2),
                    ..Default::default()
                },
            )]
            .into_iter()
            .collect(),
        };
        (configured, roots, entries, data)
    }

    fn add_ingestion_transforms(configured: &mut CandidateConfigurationInventory) {
        use asap_types::ir::scalar::{ProjectItem, ScalarExpr};
        use asap_types::ir::TimeRangeKind;
        let chains: Vec<_> = configured
            .inventory
            .templates
            .iter()
            .enumerate()
            .filter_map(|(id, node)| {
                matches!(
                    node.as_ref().unwrap().asap(),
                    Some(ASAPOp::PaneChain { .. })
                )
                .then_some(id)
            })
            .collect();
        let mut projects = Vec::new();
        for (index, &chain) in chains.iter().enumerate() {
            let scan = configured.inventory.graph.candidates[chain].producer_requirements[0]
                .compatible_producers[0];
            let input = configured.inventory.templates[scan.0]
                .as_ref()
                .unwrap()
                .clone();
            let range =
                OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::TimeRange {
                    range: std::time::Duration::from_secs(300 + index as u64 * 300),
                    kind: TimeRangeKind::Instant,
                    child: input,
                }))
                .unwrap();
            let project =
                OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Project {
                    cols: range
                        .schema
                        .fields
                        .iter()
                        .enumerate()
                        .map(|(column, field)| ProjectItem {
                            alias: (index != 0).then(|| field.name.clone()),
                            expr: ScalarExpr::Column(column),
                        })
                        .collect(),
                    qualifier: None,
                    child: range.clone(),
                }))
                .unwrap();
            let mut producer = scan;
            for node in [range, project] {
                let id = PhysicalCandidateId(configured.configurations.len());
                configured.configurations.push(CandidateConfiguration {
                    source_candidate: id,
                    materialization: Materialization::NotMaterialized,
                    timing: ExecutionTiming::IngestionTime,
                });
                configured.inventory.templates.push(Some(node));
                configured
                    .inventory
                    .graph
                    .candidates
                    .push(PhysicalCandidate {
                        group: MemoGroupId(id.0 as u32),
                        local_cost: 0.0,
                        producer_requirements: vec![ProducerRequirement {
                            compatible_producers: vec![producer],
                        }],
                    });
                producer = id;
            }
            projects.push(producer);
        }
        for chain in chains {
            configured.inventory.graph.candidates[chain].producer_requirements[0]
                .compatible_producers = projects.clone();
            let child = configured.inventory.templates[projects[0].0]
                .as_ref()
                .unwrap()
                .clone();
            let template = configured.inventory.templates[chain].as_ref().unwrap();
            configured.inventory.templates[chain] = Some(Rc::new(
                template.with_new_children(|_| child.clone()).unwrap(),
            ));
        }
    }

    // Every sharing/separate-state selection is bounded by the real evaluator,
    // including frozen calibration and measured DDSketch operation prices.
    #[test]
    fn every_tiny_selection_has_positive_certified_ingestion_and_reader_bound() {
        check_tiny_bounds(false);
    }

    // Fixed exact accumulator state is not resized by failure-probability rules;
    // every sharing/independent plan must have positive certified state pricing.
    #[test]
    fn exact_accumulator_panes_and_finalizers_have_certified_bounds() {
        check_tiny_bounds(true);
    }

    fn check_tiny_bounds(exact: bool) {
        for (measured, ranged, transformed) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (true, true, false),
            (false, false, true),
            (true, true, true),
        ] {
            let (mut configured, roots, mut entries, data) = fixture_for(exact);
            if transformed {
                add_ingestion_transforms(&mut configured);
            }
            if ranged {
                entries[0].recurrence = QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
                    RepetitionInterval(120_000),
                ));
                entries[0].range_evaluation = Some(RangeEvaluation {
                    span_ms: 120_000,
                    step_ms: 60_000,
                });
                entries[1].range_evaluation = Some(RangeEvaluation {
                    span_ms: 240_000,
                    step_ms: 120_000,
                });
            }
            let mut settings = crate::PlanSelectionSettings::default();
            if measured {
                settings.measured_operation_costs = serde_json::from_value(serde_json::json!({"operators": [{"operator": "PaneChain", "phase": "ingestion_time", "sketch": {"DDSketch": {"alpha": 0.1}}, "ns_per_unit": 300.0, "ns_per_invocation": 200.0, "memory_bytes_per_state": 1000.0}, {"operator": "SummaryMerge", "phase": "query_time", "sketch": {"DDSketch": {"alpha": 0.1}}, "ns_per_unit": 500.0, "ns_per_invocation": 100.0}, {"operator": "SummaryEstimate", "phase": "query_time", "sketch": {"DDSketch": {"alpha": 0.1}}, "ns_per_unit": 900.0, "ns_per_invocation": 400.0}]})).unwrap();
            }
            if exact && measured {
                settings.measured_operation_costs = serde_json::from_value(serde_json::json!({"operators": [
                    {"operator": "PaneChain", "phase": "ingestion_time", "exact": "Sum", "ns_per_unit": 300.0, "ns_per_invocation": 200.0, "memory_bytes_per_state": 64.0},
                    {"operator": "SummaryMerge", "phase": "query_time", "exact": "Sum", "ns_per_unit": 500.0, "ns_per_invocation": 100.0},
                    {"operator": "FinalizeExactAccumulator", "phase": "query_time", "exact": "Sum", "ns_per_unit": 900.0, "ns_per_invocation": 400.0}
                ]})).unwrap();
            }
            let models = PlanningModels::builtin().with_plan_selection(&settings);
            let calibration = CalibrationFit {
                measured_ms_per_second: 2.0,
                formula_ms_per_second: 1.0,
            };
            let context = CandidatePlanningContext {
                original_roots: &roots,
                entries: &entries,
                data: &data,
                models,
                calibration,
            };
            let metrics = apply(&mut configured, &context).unwrap();
            assert_eq!(metrics.ingestion_pane_chains, 2);
            assert!(metrics.ingestion_scans > 0);
            assert!(metrics.query_readers > 0);
            if exact {
                let finalizers: Vec<_> = configured
                    .inventory
                    .templates
                    .iter()
                    .enumerate()
                    .filter_map(|(id, template)| {
                        matches!(
                            template.as_ref().unwrap().asap(),
                            Some(ASAPOp::FinalizeExactAccumulator { .. })
                        )
                        .then_some(id)
                    })
                    .collect();
                assert!(!finalizers.is_empty());
                assert!(finalizers
                    .iter()
                    .all(|&id| configured.inventory.graph.candidates[id].local_cost > 0.0));
            }
            if transformed {
                assert_eq!(metrics.ingestion_transforms, 4);
            }
            let mut observations = Vec::new();
            let mut valid = 0;
            while let Some(proposal) =
                solve_candidate_proposal(&configured.inventory.graph, &observations, None).unwrap()
            {
                let selected = proposal.selected;
                let mut modes = vec![None; configured.configurations.len()];
                for id in &selected.selected_candidates {
                    modes[id.0] = Some(configured.configurations[id.0].materialization);
                }
                let physical = materialize_selected_candidates(
                    &configured.inventory,
                    &selected,
                    &modes,
                    &roots,
                    &entries,
                    &data,
                    models.deployment_capabilities,
                )
                .unwrap();
                let mut selector = PlanSelector::new(&entries, &data, models, false).unwrap();
                selector.set_calibration(calibration);
                selector.offer(&physical.physical).unwrap();
                let (result, _) = selector.finish().unwrap();
                let cost = result.costs[&result.selected].cost_per_second.unwrap();
                assert!(selected.modeled_cost > 0.0);
                assert!(
                    selected.modeled_cost <= cost * (1.0 + 1e-9),
                    "bound {} > cost {cost}",
                    selected.modeled_cost
                );
                valid += 1;
                observations.push(ObservedAssignment {
                    selected,
                    exact_cost: None,
                    rejected_queries: Vec::new(),
                });
                assert!(valid <= 16, "bounded tiny supplier choices");
            }
            assert_eq!(valid, if transformed { 12 } else { 4 });
        }
    }
    // Any potential retained consumer suppresses upstream query-work bounds,
    // while independent fixed-ingestion state remains safely priceable.
    #[test]
    fn retained_consumer_context_keeps_query_work_unpriced() {
        let (mut configured, roots, entries, data) = fixture();
        for root in &configured.inventory.graph.query_roots {
            for candidate in &root.compatible_candidates {
                configured.configurations[candidate.0].materialization =
                    Materialization::MatAtQueryKeptAcrossEvaluations(Medium::Memory);
            }
        }
        let context = CandidatePlanningContext {
            original_roots: &roots,
            entries: &entries,
            data: &data,
            models: PlanningModels::builtin(),
            calibration: CalibrationFit::default(),
        };
        let metrics = apply(&mut configured, &context).unwrap();
        assert_eq!(metrics.query_readers, 0);
        assert!(metrics.ingestion_pane_chains > 0);
        for (candidate, config) in configured
            .inventory
            .graph
            .candidates
            .iter()
            .zip(&configured.configurations)
        {
            if config.timing == ExecutionTiming::QueryTime {
                assert_eq!(candidate.local_cost, 0.0);
            }
        }
    }
    // Equal row counts do not certify different sources as interchangeable cost inputs.
    #[test]
    fn differing_supplier_metadata_leaves_pane_bounds_zero() {
        let (mut configured, roots, entries, mut data) = fixture();
        let scans: Vec<_> = configured
            .inventory
            .templates
            .iter()
            .enumerate()
            .filter_map(|(id, node)| {
                matches!(
                    node.as_ref().unwrap().non_asap(),
                    Some(NonASAPOp::Scan { .. })
                )
                .then_some(PhysicalCandidateId(id))
            })
            .collect();
        assert_eq!(scans.len(), 2);
        let original = configured.inventory.templates[scans[1].0].as_ref().unwrap();
        let mut different = (**original).clone();
        let asap_types::ir::Operator::NonASAP(NonASAPOp::Scan { source, .. }) =
            &mut different.operator
        else {
            unreachable!()
        };
        *source = asap_types::ir::operator::Source::TimeSeries {
            metric: "other".into(),
        };
        configured.inventory.templates[scans[1].0] = Some(Rc::new(different));
        data.sources
            .insert("other".into(), data.sources["latency"].clone());
        for (candidate, node) in configured
            .inventory
            .graph
            .candidates
            .iter_mut()
            .zip(&configured.inventory.templates)
        {
            if matches!(
                node.as_ref().unwrap().asap(),
                Some(ASAPOp::PaneChain { .. })
            ) {
                candidate.producer_requirements[0].compatible_producers = scans.clone();
            }
        }
        let context = CandidatePlanningContext {
            original_roots: &roots,
            entries: &entries,
            data: &data,
            models: PlanningModels::builtin(),
            calibration: CalibrationFit::default(),
        };
        let metrics = apply(&mut configured, &context).unwrap();
        assert_eq!(metrics.ingestion_pane_chains, 0);
        assert_eq!(metrics.ingestion_scans, 2);
    }
}
