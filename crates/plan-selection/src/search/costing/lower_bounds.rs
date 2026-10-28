//! Certified node-local usage bounds for fixed DDS/exact state and its query readers.
//! Consumer-dependent held memory, raw retention, request fees and extra copies
//! remain in the complete evaluator. Unsupported pricing contexts contribute zero.
use asap_types::ir::physical_export::PhysicalASAPDAGNode;
use asap_types::ir::properties::execution_data_state::{DataPrimitive, ExecutionDataState};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::schema::{FieldDataType, SketchParams};
use asap_types::ir::{ASAPOp, NonASAPOp};
use asap_types::physical::Materialization;

use crate::search::candidates::configurations::CandidateConfigurationInventory;
use crate::search::refinement::planner::CandidatePlanningContext;
use crate::{price_node, CostModel, PricedNode, ProducerOutput, StateFamily};

#[derive(Debug, Default, Clone, Copy)]
pub(in crate::search) struct LowerBoundMetrics {
    pub priced_candidates: usize,
    pub rounded_down_candidates: usize,
    pub ingestion_scans: usize,
    pub ingestion_pane_chains: usize,
    pub query_readers: usize,
    pub ingestion_transforms: usize,
}

/// Replace placeholder costs in the actual inventory used by selection and
/// reconstruction. This is a lower bound, never a substitute for plan evaluation.
pub(in crate::search) fn apply(
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
        let lookbacks = shortest_reader_windows(configured);
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
                        // A query-time scan reads at least its shortest
                        // reader window ([`shortest_reader_windows`]); the
                        // row operators and builds above it read at least
                        // the rows that bound certifies.
                        (Some(NonASAPOp::Scan { .. }), _) if lookbacks[id].is_none() => {
                            return None
                        }
                        (
                            Some(
                                NonASAPOp::Project { .. }
                                | NonASAPOp::Scan { .. }
                                | NonASAPOp::TimeRange { .. }
                                | NonASAPOp::TimeShift { .. }
                                | NonASAPOp::Aggregate { .. },
                            ),
                            _,
                        )
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
                        (
                            _,
                            Some(build @ (ASAPOp::SummaryAgg { .. } | ASAPOp::PaneChain { .. })),
                        ) if template_price_is_a_floor(
                            build,
                            ExecutionTiming::QueryTime,
                            context,
                        ) =>
                        {
                            ExecutionDataState {
                                timing: ExecutionTiming::QueryTime,
                                primitive: DataPrimitive::SummaryState,
                            }
                        }
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
                            Some(
                                chain @ ASAPOp::PaneChain {
                                    family: FieldDataType::Sketch(..),
                                    ..
                                },
                            ),
                            Materialization::MatAtIngestion(_),
                        ) if template_price_is_a_floor(
                            chain,
                            ExecutionTiming::IngestionTime,
                            context,
                        ) =>
                        {
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
        let concrete_rows = ConcreteRows::of(configured);
        let concrete_prices = concrete_rows.price(context);
        let certified_rows = concrete_rows.certify(configured, &concrete_prices)?;
        let mut cache: Vec<_> = certified_rows
            .iter()
            .enumerate()
            .map(|(id, &certified)| {
                certified
                    .then(|| concrete_prices[concrete_rows.roots[id].unwrap()].clone())
                    .flatten()
            })
            .collect();
        let mut visiting = vec![false; count];
        let mut complete = certified_rows;
        for id in 0..count {
            if let Some(priced) = certified_price(
                id,
                configured,
                &nodes,
                &lookbacks,
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
    // The solver's coefficient range is narrower than the full pane domain.
    // Zero is still a certified lower bound for omitted tiny local charges;
    // the evaluator continues to charge their complete actual cost.
    metrics.rounded_down_candidates = round_down_small_bounds(&mut costs);
    metrics.priced_candidates -= metrics.rounded_down_candidates;
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

/// Each candidate's shortest reader window: the least window any candidate
/// that may read it (through time shifts) reads, as the evaluator's
/// `windows_read` counts windows (a time range's range, a pane chain's
/// panes). A query-time scan reads its readers' longest window, or every
/// row held when no reader has one; both are at least the rows of this
/// window capped by the rows held. `None` when no reader has a window.
fn shortest_reader_windows(configured: &CandidateConfigurationInventory) -> Vec<Option<u64>> {
    let graph = &configured.inventory.graph;
    let mut readers = vec![Vec::new(); graph.candidates.len()];
    for (reader, candidate) in graph.candidates.iter().enumerate() {
        for requirement in &candidate.producer_requirements {
            for producer in requirement.compatible_producers.iter() {
                readers[producer.0].push(reader);
            }
        }
    }
    (0..graph.candidates.len())
        .map(|id| {
            let mut shortest: Option<u64> = None;
            let mut pending = vec![id];
            let mut seen = std::collections::HashSet::new();
            while let Some(producer) = pending.pop() {
                if !seen.insert(producer) {
                    continue;
                }
                for &reader in &readers[producer] {
                    let window = match configured.inventory.templates[reader]
                        .as_ref()
                        .map(|template| &template.operator)
                    {
                        Some(asap_types::ir::Operator::NonASAP(NonASAPOp::TimeRange {
                            range,
                            ..
                        })) => Some(range.as_millis() as u64),
                        Some(asap_types::ir::Operator::NonASAP(NonASAPOp::TimeShift {
                            ..
                        })) => {
                            pending.push(reader);
                            None
                        }
                        // A chain kept across evaluations reads one pane;
                        // its scans are never certified (retained ancestors).
                        Some(asap_types::ir::Operator::ASAP(ASAPOp::PaneChain {
                            pane_width_ms,
                            pane_count,
                            ..
                        })) => Some(pane_width_ms.saturating_mul(u64::from(*pane_count))),
                        _ => None,
                    };
                    if let Some(window) = window {
                        shortest = Some(shortest.map_or(window, |least| least.min(window)));
                    }
                }
            }
            shortest
        })
        .collect()
}

/// Whether a summary build's price at its template's parameters is at most
/// its price in any plan that selects it. Stage 3 resizes a sketch only to
/// larger parameters (`with_failure_probability` and HLL's `min_precision`
/// keep at least the current size), and the analytical shape
/// (`sketch_shape`) never charges a larger sketch less. A measured table
/// interpolates between measured sizes and need not grow with size, so a
/// resizable sketch with measured entries is not certified. DDSketch and
/// exact accumulators are never resized.
fn template_price_is_a_floor(
    build: &ASAPOp,
    timing: ExecutionTiming,
    context: &CandidatePlanningContext<'_>,
) -> bool {
    let (ASAPOp::SummaryAgg { family, .. } | ASAPOp::PaneChain { family, .. }) = build else {
        return false;
    };
    let FieldDataType::Sketch(kind, _) = family else {
        return true;
    };
    let params = kind.params();
    if matches!(params, SketchParams::DDSketch { .. }) {
        return true;
    }
    use crate::cost::measured::{lookup, OperatorKind, StateFamily};
    [OperatorKind::SummaryAgg, OperatorKind::PaneChain]
        .into_iter()
        .all(|operator| {
            lookup(
                &context.models.plan_selection.measured_operation_costs,
                operator,
                timing,
                StateFamily::Sketch(params),
                |_| 1.0,
            )
            .is_none()
        })
}

fn round_down_small_bounds(costs: &mut [f64]) -> usize {
    let largest = costs.iter().copied().fold(0.0_f64, f64::max);
    let mut removed = 0;
    for cost in costs {
        if *cost > 0.0
            && *cost / largest < crate::search::candidates::graph::MINIMUM_RELATIVE_COEFFICIENT
        {
            *cost = 0.0;
            removed += 1;
        }
    }
    removed
}

// Potential source-widening choices may cycle although every selected execution
// DAG is acyclic. Certify their row interfaces locally, rather than interpreting
// a recursive visit of a potential cycle as an unknown physical input.
struct ConcreteRows {
    nodes: Vec<PhysicalASAPDAGNode>,
    children: Vec<Vec<usize>>,
    roots: Vec<Option<usize>>,
}

impl ConcreteRows {
    fn of(configured: &CandidateConfigurationInventory) -> Self {
        let mut result = Self {
            nodes: Vec::new(),
            children: Vec::new(),
            roots: Vec::new(),
        };
        fn append(node: &asap_types::ir::OperatorNode, result: &mut ConcreteRows) -> Option<usize> {
            if !matches!(
                node.non_asap(),
                Some(
                    NonASAPOp::Scan { .. }
                        | NonASAPOp::Project { .. }
                        | NonASAPOp::TimeRange { .. }
                )
            ) {
                return None;
            }
            let mut children = Vec::new();
            let mut supported = true;
            let payload = node.operator.map_children(|child| {
                let index = children.len();
                match append(child, result) {
                    Some(child) => children.push(child),
                    None => supported = false,
                }
                index
            });
            if !supported {
                return None;
            }
            let id = result.nodes.len();
            result.nodes.push(PhysicalASAPDAGNode {
                id,
                payload,
                output_state: ExecutionDataState::INGESTION_ROWS,
                materialization: Materialization::NotMaterialized,
                output_schema: node.schema.clone(),
                guarantee: node.guarantee.clone(),
            });
            result.children.push(children);
            Some(id)
        }
        for (id, config) in configured.configurations.iter().enumerate() {
            let root = if config.timing == ExecutionTiming::IngestionTime
                && config.materialization == Materialization::NotMaterialized
            {
                configured.inventory.templates[id]
                    .as_ref()
                    .and_then(|node| append(node, &mut result))
            } else {
                None
            };
            result.roots.push(root);
        }
        result
    }

    fn price<'a>(
        &'a self,
        context: &'a CandidatePlanningContext<'a>,
    ) -> Vec<Option<PricedNode<'a>>> {
        let mut result: Vec<Option<PricedNode<'a>>> = Vec::with_capacity(self.nodes.len());
        for (id, node) in self.nodes.iter().enumerate() {
            let views: Option<Vec<_>> = self.children[id]
                .iter()
                .map(|&child| {
                    let price = result[child].as_ref()?;
                    Some(ProducerOutput {
                        payload: &self.nodes[child].payload,
                        output_schema: &self.nodes[child].output_schema,
                        output: price.output,
                        sources: price.sources.clone(),
                        family: price.family,
                        unchanged_columns: price.unchanged_columns.clone(),
                    })
                })
                .collect();
            result.push(views.and_then(|views| {
                price_node(
                    node,
                    &views,
                    (1, 1.0),
                    asap_types::ir::physical_export::ScanWindow::Unbounded,
                    1,
                    None,
                    context.data,
                    context.models.plan_selection,
                    context.calibration.factor(),
                )
                .ok()
            }));
        }
        result
    }

    fn certify(
        &self,
        configured: &CandidateConfigurationInventory,
        prices: &[Option<PricedNode<'_>>],
    ) -> Result<Vec<bool>, String> {
        let interfaces: Vec<_> = self
            .nodes
            .iter()
            .zip(prices)
            .map(|(node, priced)| {
                priced
                    .as_ref()
                    .map(|priced| {
                        // Open→closed schema refinement changes no row pricing inputs.
                        let mut schema = node.output_schema.clone();
                        schema.closed = false;
                        serde_json::to_string(&(
                            schema,
                            priced.output,
                            &priced.sources,
                            &priced.unchanged_columns,
                        ))
                        .map_err(|error| error.to_string())
                    })
                    .transpose()
            })
            .collect::<Result<_, _>>()?;
        let mut valid: Vec<_> = self
            .roots
            .iter()
            .map(|root| root.is_some_and(|root| interfaces[root].is_some()))
            .collect();
        loop {
            let mut changed = false;
            for id in 0..valid.len() {
                if !valid[id] {
                    continue;
                }
                let root = self.roots[id].unwrap();
                let inputs = &configured.inventory.graph.candidates[id].producer_requirements;
                let agrees = inputs.len() == self.children[root].len()
                    && inputs
                        .iter()
                        .zip(&self.children[root])
                        .all(|(input, &child)| {
                            !input.compatible_producers.is_empty()
                                && input.compatible_producers.iter().all(|supplier| {
                                    valid[supplier.0]
                                        && interfaces[self.roots[supplier.0].unwrap()]
                                            == interfaces[child]
                                })
                        });
                if !agrees {
                    valid[id] = false;
                    changed = true;
                }
            }
            if !changed {
                return Ok(valid);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn certified_price<'a>(
    id: usize,
    configured: &CandidateConfigurationInventory,
    nodes: &'a [Option<PhysicalASAPDAGNode>],
    lookbacks: &[Option<u64>],
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
                    lookbacks,
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
                // DDS state bytes depend on whether the selected producer
                // exposes nonnegative raw samples. Equal row statistics alone
                // cannot certify Scan and Project as interchangeable DDS inputs.
                // Other whitelisted operators do not use this domain flag.
                let raw_nonnegative = matches!(
                    &node.payload,
                    asap_types::ir::Operator::ASAP(ASAPOp::PaneChain {
                        family: FieldDataType::Sketch(kind, _), ..
                    }) if matches!(kind.params(), SketchParams::DDSketch { .. })
                )
                .then(|| {
                    matches!(
                        producer.payload,
                        asap_types::ir::Operator::NonASAP(
                            NonASAPOp::Scan { .. } | NonASAPOp::TimeRange { .. }
                        )
                    ) && priced
                        .sources
                        .iter()
                        .all(|source| crate::declared_non_negative(context.data, source))
                });
                // Producer pane counts, family, schema fields, statistics and
                // provenance must agree; upstream work is charged separately.
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
                let mut schema = producer.output_schema.clone();
                schema.closed = false;
                let current = serde_json::to_string(&(
                    raw_nonnegative,
                    panes,
                    family,
                    schema,
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
        let maintained_window = match &node.payload {
            asap_types::ir::Operator::ASAP(ASAPOp::PaneChain {
                pane_width_ms,
                pane_count,
                ..
            }) => Some(pane_width_ms.saturating_mul(u64::from(*pane_count))),
            _ => None,
        };
        let retained = match &node.payload {
            asap_types::ir::Operator::ASAP(ASAPOp::PaneChain { pane_count, .. }) => {
                u64::from(*pane_count).max(1)
            }
            _ => 1,
        };
        // One ingestion boundary executes once per accounted second; additional
        // unmaterialized consumers can only add executions. A summary build is
        // priced at its template's size, the smallest Stage 3 can give it
        // ([`template_price_is_a_floor`]). Retention above its own pane count
        // and all whole-plan charges are deliberately omitted.
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
            // Stage 3 scans the width of the union of the readers' windows,
            // at least the shortest one, whatever their offsets.
            crate::trailing_window(lookbacks[id]),
            retained,
            maintained_window,
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
    // Fine panes may widen the bound range; round down only the relaxation,
    // preserving a valid lower bound and the core solver's numeric contract.
    #[test]
    fn tiny_certificates_are_rounded_down_without_inflating_other_costs() {
        let original = [0.0, 1e-12, 1e-9, 1.0];
        let mut costs = original;
        assert_eq!(super::round_down_small_bounds(&mut costs), 1);
        assert_eq!(costs, [0.0, 0.0, 1e-9, 1.0]);
        assert!(costs
            .iter()
            .zip(original)
            .all(|(bound, actual)| *bound <= actual));
        assert_eq!(super::round_down_small_bounds(&mut [0.0, 0.0]), 0);
    }

    use super::*;
    use crate::search::candidates::configurations::CandidateConfiguration;
    use crate::search::candidates::graph::{
        CandidateDependencyGraph, PhysicalCandidate, PhysicalCandidateId, ProducerRequirement,
        QueryRootCandidates,
    };
    use crate::search::candidates::inventory::CandidateInventory;
    use crate::search::candidates::physical::materialize_selected_candidates;
    use crate::search::milp::model::{solve_candidate_proposal, ObservedAssignment};
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
        fixture_for_grouping(exact, false)
    }

    fn fixture_for_grouping(
        exact: bool,
        grouped: bool,
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
            let query = if grouped {
                "sum(sum_over_time(latency[5m]))".to_string()
            } else if exact {
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
                    matches!(node.asap(), Some(ASAPOp::SummaryAgg { family: FieldDataType::ExactAggregate(..), child, .. }) if !grouped || matches!(child.non_asap(), Some(NonASAPOp::TimeRange { .. })))
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
                            compatible_producers: vec![PhysicalCandidateId(input)].into(),
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
                    input.compatible_producers = chains.clone().into();
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
                    values_non_negative: true,
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
                            compatible_producers: vec![producer].into(),
                        }],
                    });
                producer = id;
            }
            projects.push(producer);
        }
        for chain in chains {
            configured.inventory.graph.candidates[chain].producer_requirements[0]
                .compatible_producers = projects.clone().into();
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

    // Equal row schemas/statistics do not make Scan and Project equivalent
    // DDS pricing inputs: nonnegative raw samples need only one bin domain.
    #[test]
    fn mixed_raw_and_project_inputs_never_overprice_nonnegative_dds() {
        let (mut configured, roots, entries, mut data) = fixture();
        data.sources.get_mut("latency").unwrap().values_non_negative = true;
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
        let mut projects = Vec::new();
        for scan in &scans {
            let input = configured.inventory.templates[scan.0]
                .as_ref()
                .unwrap()
                .clone();
            let project =
                OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Project {
                    cols: input
                        .schema
                        .fields
                        .iter()
                        .enumerate()
                        .map(|(column, field)| asap_types::ir::scalar::ProjectItem {
                            alias: Some(field.name.clone()),
                            expr: asap_types::ir::scalar::ScalarExpr::Column(column),
                        })
                        .collect(),
                    qualifier: None,
                    child: input,
                }))
                .unwrap();
            let id = PhysicalCandidateId(configured.configurations.len());
            configured.configurations.push(CandidateConfiguration {
                source_candidate: id,
                materialization: Materialization::NotMaterialized,
                timing: ExecutionTiming::IngestionTime,
            });
            configured.inventory.templates.push(Some(project));
            configured
                .inventory
                .graph
                .candidates
                .push(PhysicalCandidate {
                    group: MemoGroupId(id.0 as u32),
                    local_cost: 0.0,
                    producer_requirements: vec![ProducerRequirement {
                        compatible_producers: vec![*scan].into(),
                    }],
                });
            projects.push(id);
        }
        for (id, template) in configured.inventory.templates.iter().enumerate() {
            if matches!(
                template.as_ref().unwrap().asap(),
                Some(ASAPOp::PaneChain { .. })
            ) {
                let input = &mut configured.inventory.graph.candidates[id].producer_requirements[0];
                // Put larger signed-domain Project pricing last to expose the
                // former arbitrary representative selection.
                let mut suppliers = scans.clone();
                suppliers.extend(projects.iter().copied());
                input.compatible_producers = suppliers.into();
            }
        }
        let models = PlanningModels::builtin();
        let calibration = CalibrationFit::default();
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            seed_separate_plans: false,
            original_roots: &roots,
            entries: &entries,
            data: &data,
            models,
            calibration,
        };
        let metrics = apply(&mut configured, &context).unwrap();
        assert!(metrics.ingestion_scans > 0);
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
            assert!(valid <= 128);
        }
        assert!(
            valid > 12,
            "both raw and transformed supplier bindings were checked"
        );
    }

    // Every sharing/separate-state selection is bounded by the real evaluator,
    // including frozen calibration and measured DDSketch operation prices.
    #[test]
    fn every_tiny_selection_has_positive_certified_ingestion_and_reader_bound() {
        check_tiny_bounds(false, false, false);
    }

    // Fixed exact accumulator state is not resized by failure-probability rules;
    // every sharing/independent plan must have positive certified state pricing.
    #[test]
    fn exact_accumulator_panes_and_finalizers_have_certified_bounds() {
        check_tiny_bounds(true, false, false);
    }

    // Potential identity-projection cycles are legal catalog choices, but only
    // acyclic selected connections execute. Local row-interface certificates
    // preserve positive bounds for every finite acyclic choice through them.
    #[test]
    fn potential_projection_cycles_do_not_poison_certified_state_bounds() {
        check_tiny_bounds(false, true, false);
        check_tiny_bounds(true, true, false);
    }

    // Grouped ingestion state uses its own exact pane horizon, matching the
    // complete evaluator's maintained_window_ms rather than an unknown window.
    #[test]
    fn grouped_exact_panes_use_their_declared_maintenance_horizon() {
        check_tiny_bounds(true, false, true);
    }

    fn check_tiny_bounds(exact: bool, cyclic: bool, grouped: bool) {
        for (measured, ranged, transformed) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (true, true, false),
            (false, false, true),
            (true, true, true),
        ] {
            let (mut configured, roots, mut entries, data) = fixture_for_grouping(exact, grouped);
            if cyclic && !transformed {
                continue;
            }
            if transformed {
                add_ingestion_transforms(&mut configured);
                if cyclic {
                    let projects: Vec<_> = configured
                        .inventory
                        .templates
                        .iter()
                        .enumerate()
                        .filter_map(|(id, node)| {
                            (configured.configurations[id].timing == ExecutionTiming::IngestionTime
                                && matches!(
                                    node.as_ref().unwrap().non_asap(),
                                    Some(NonASAPOp::Project { .. })
                                ))
                            .then_some(PhysicalCandidateId(id))
                        })
                        .collect();
                    for project in &projects {
                        configured.inventory.graph.candidates[project.0].producer_requirements[0]
                            .compatible_producers
                            .extend(projects.iter().copied());
                    }
                }
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
                deadline: None,
                refine_materialization: false,
                seed_separate_plans: false,
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
                assert!(valid <= 128, "bounded tiny supplier choices");
            }
            if cyclic {
                assert!(
                    valid > 12,
                    "cycles expose additional acyclic connection choices"
                );
            } else {
                assert_eq!(valid, if transformed { 12 } else { 4 });
            }
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
            deadline: None,
            refine_materialization: false,
            seed_separate_plans: false,
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
                candidate.producer_requirements[0].compatible_producers = scans.clone().into();
            }
        }
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            seed_separate_plans: false,
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

    /// `query`'s Pass 1 candidate that `pick` selects, built for `built_for`,
    /// every node at query time: its configured inventory after [`apply`],
    /// the bound of its selected plan, and that plan's evaluated cost for
    /// queries with target `evaluated_for` (Stage 3 resizes sketches to it).
    fn query_time_bound_and_cost(
        query: &str,
        built_for: AccuracyTarget,
        evaluated_for: AccuracyTarget,
        settings: &crate::PlanSelectionSettings,
        pick: fn(&OperatorNode) -> bool,
    ) -> (CandidateConfigurationInventory, f64, f64) {
        let roots_with_ids = vec![(0usize, QueryRoot::Operator(lower_promql(query, built_for)))];
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
        let candidate = memo.candidates[root_group]
            .iter()
            .find(|candidate| {
                OperatorNode::reachable(&candidate.node)
                    .iter()
                    .any(|node| pick(node))
            })
            .unwrap();
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
        ) -> usize {
            let mut inputs = Vec::new();
            node.operator.map_children(|child| {
                inputs.push(intern(child, configured));
                Rc::clone(child)
            });
            let id = configured.inventory.templates.len();
            configured.configurations.push(CandidateConfiguration {
                source_candidate: PhysicalCandidateId(id),
                timing: ExecutionTiming::QueryTime,
                materialization: Materialization::NotMaterialized,
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
                            compatible_producers: vec![PhysicalCandidateId(input)].into(),
                        })
                        .collect(),
                });
            id
        }
        let root = intern(&candidate.node, &mut configured);
        configured
            .inventory
            .graph
            .query_roots
            .push(QueryRootCandidates {
                compatible_candidates: vec![PhysicalCandidateId(root)],
            });
        let roots = vec![QueryRoot::Operator(Rc::clone(&candidate.node))];
        let entries = vec![entry_with(
            evaluated_for,
            QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(10_000))),
        )];
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
        let models = PlanningModels::builtin().with_plan_selection(settings);
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            original_roots: &roots,
            entries: &entries,
            data: &data,
            models,
            calibration: CalibrationFit::default(),
            seed_separate_plans: false,
        };
        apply(&mut configured, &context).unwrap();
        let selected = solve_candidate_proposal(&configured.inventory.graph, &[], None)
            .unwrap()
            .unwrap()
            .selected;
        let modes: Vec<_> = configured
            .configurations
            .iter()
            .map(|config| Some(config.materialization))
            .collect();
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
        selector.offer(&physical.physical).unwrap();
        let (result, _) = selector.finish().unwrap();
        let cost = result.costs[&result.selected].cost_per_second.unwrap();
        (configured, selected.modeled_cost, cost)
    }

    fn bound_of(
        configured: &CandidateConfigurationInventory,
        matches: impl Fn(&OperatorNode) -> bool,
    ) -> f64 {
        configured
            .inventory
            .templates
            .iter()
            .zip(&configured.inventory.graph.candidates)
            .find(|(template, _)| matches(template.as_ref().unwrap()))
            .unwrap()
            .1
            .local_cost
    }

    // A query-time scan, its window and the exact build over it are priced
    // from the window's rows at the slowest query rate: positive bounds that
    // never exceed the evaluated plan's cost.
    #[test]
    fn query_time_scans_and_builds_have_certified_bounds() {
        let (configured, bound, cost) = query_time_bound_and_cost(
            "sum_over_time(latency[5m])",
            AccuracyTarget::exact(),
            AccuracyTarget::exact(),
            &crate::PlanSelectionSettings::default(),
            |node| {
                matches!(node.asap(), Some(ASAPOp::SummaryAgg { family: FieldDataType::ExactAggregate(..), child, .. })
                    if matches!(child.non_asap(), Some(NonASAPOp::TimeRange { .. })))
            },
        );
        let scan = bound_of(&configured, |node| {
            matches!(node.non_asap(), Some(NonASAPOp::Scan { .. }))
        });
        let range = bound_of(&configured, |node| {
            matches!(node.non_asap(), Some(NonASAPOp::TimeRange { .. }))
        });
        let build = bound_of(&configured, |node| {
            matches!(node.asap(), Some(ASAPOp::SummaryAgg { .. }))
        });
        // 300 s × 1000 rows/s, every 10 s: 30000 rows read per second.
        assert!(
            scan > 0.0 && range > 0.0 && build > 0.0,
            "{scan} {range} {build}"
        );
        assert!(
            bound > 0.0 && bound <= cost * (1.0 + 1e-9),
            "bound {bound} against cost {cost}"
        );
    }

    fn is_kll_build(node: &OperatorNode) -> bool {
        matches!(node.asap(), Some(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. })
            if matches!(kind.params(), SketchParams::Kll { .. }))
    }

    // A KLL build is priced at its template's size, the smallest Stage 3
    // gives it: a smaller failure probability (as a smaller budget share
    // gives) resizes it larger, and the bound stays below every such cost.
    #[test]
    fn a_kll_build_bound_holds_for_every_resized_size() {
        let built_for = AccuracyTarget::max_error(0.01).failure_probability(0.01);
        let mut costs = Vec::new();
        for failure_probability in [0.01, 0.001, 0.0001] {
            let (configured, bound, cost) = query_time_bound_and_cost(
                "quantile_over_time(0.9, latency[5m])",
                built_for.clone(),
                AccuracyTarget::max_error(0.01).failure_probability(failure_probability),
                &crate::PlanSelectionSettings::default(),
                is_kll_build,
            );
            assert!(bound_of(&configured, is_kll_build) > 0.0);
            assert!(
                bound > 0.0 && bound <= cost * (1.0 + 1e-9),
                "bound {bound} against cost {cost} at failure probability {failure_probability}"
            );
            costs.push(cost);
        }
        assert!(
            costs[2] > costs[0],
            "resizing must enlarge the sketch: {costs:?}"
        );
    }

    // A measured table need not grow with size, so a resizable sketch with
    // measured entries gets no certified bound.
    #[test]
    fn a_measured_kll_build_is_not_certified() {
        let settings = crate::PlanSelectionSettings {
            measured_operation_costs: serde_json::from_value(serde_json::json!({"operators": [
            {"operator": "SummaryAgg", "phase": "query_time", "sketch": {"Kll": {"k": 200}}, "ns_per_unit": 300.0, "ns_per_invocation": 200.0}
        ]}))
        .unwrap(),
            ..Default::default()
        };
        let (configured, _, _) = query_time_bound_and_cost(
            "quantile_over_time(0.9, latency[5m])",
            AccuracyTarget::max_error(0.01).failure_probability(0.01),
            AccuracyTarget::max_error(0.01).failure_probability(0.01),
            &settings,
            is_kll_build,
        );
        assert_eq!(bound_of(&configured, is_kll_build), 0.0);
    }
}
