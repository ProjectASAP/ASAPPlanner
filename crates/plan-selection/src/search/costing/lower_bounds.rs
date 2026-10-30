//! Certified node-local usage bounds for fixed DDS/exact state and its query readers.
//! Consumer-dependent held memory, raw retention, request fees and extra copies
//! remain in the complete evaluator. Unsupported pricing contexts contribute zero.
use asap_types::ir::physical_export::PhysicalASAPDAGNode;
use asap_types::ir::properties::execution_data_state::{DataPrimitive, ExecutionDataState};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::schema::{FieldDataType, SketchParams};
use asap_types::ir::{ASAPOp, NonASAPOp};
use asap_types::physical::Materialization;

use crate::search::candidates::dag::PhysicalCandidatesWithCosts;
use crate::search::refinement::planner::CandidatePlanningContext;
use crate::StateFamily;
use asap_cost::CostModel;
use asap_cost::{node_usage, NodeUsage, ProducerOutput};

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
    configured: &mut PhysicalCandidatesWithCosts,
    context: &CandidatePlanningContext<'_>,
) -> Result<LowerBoundMetrics, String> {
    configured.dag.validate().map_err(|e| e.to_string())?;
    let count = configured.dag.candidates.len();
    if configured.physical.candidates.len() != count {
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
                    asap_cost::execution_demand::evaluation_rate(
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
            .physical
            .candidates
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
                configured.dag.candidates[id]
                    .producer_candidates_per_input
                    .iter()
                    .flat_map(|input| input.candidates.iter().map(|producer| producer.0)),
            );
        }
        let common_reader_window_ms = common_reader_windows(configured);
        let nodes: Vec<_> = (0..count)
            .map(|id| {
                let configuration = &configured.physical.candidates[id];
                let template = configured.node(id);
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
                        // A query-time scan reads at least its common
                        // reader window ([`common_reader_windows`]); the
                        // row operators and builds above it read at least
                        // the rows that bound certifies.
                        (Some(NonASAPOp::Scan { .. }), _)
                            if common_reader_window_ms[id].is_none() =>
                        {
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
                        ) if template_price_is_a_floor(build, context) => ExecutionDataState {
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
                            Some(
                                chain @ ASAPOp::PaneChain {
                                    family: FieldDataType::Sketch(..),
                                    ..
                                },
                            ),
                            Materialization::MatAtIngestion(_),
                        ) if template_price_is_a_floor(chain, context) => {
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
                    != configured.dag.candidates[id]
                        .producer_candidates_per_input
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
                    retention_ms: None,
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
                &common_reader_window_ms,
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
    for (candidate, cost) in configured.dag.candidates.iter_mut().zip(costs) {
        candidate.local_cost = cost;
    }
    configured.dag.validate().map_err(|e| e.to_string())?;
    Ok(metrics)
}

/// Each candidate's common reader window: the window every candidate that
/// may read it (through time shifts) reads, as the evaluator's
/// `windows_read` counts windows (a time range's range, a pane chain's
/// panes). A query-time scan reads its readers' longest window, or every
/// row held when no reader has one; both are at least the rows of this
/// window capped by the rows held. `None` when no reader has a window, or
/// when readers' windows differ: which window the scan reads then depends on
/// the readers selected, so the scan is left uncertified like any other node
/// without a sound fixed bound. (A bound at the shortest window was sound but
/// priced a scan a short and a long window may share far below a scan for the
/// long window alone, so the search proposed the shared scan first, #994.)
fn common_reader_windows(configured: &PhysicalCandidatesWithCosts) -> Vec<Option<u64>> {
    let graph = &configured.dag;
    let mut readers = vec![Vec::new(); graph.candidates.len()];
    for (reader, candidate) in graph.candidates.iter().enumerate() {
        for requirement in &candidate.producer_candidates_per_input {
            for producer in requirement.candidates.iter() {
                readers[producer.0].push(reader);
            }
        }
    }
    (0..graph.candidates.len())
        .map(|id| {
            let mut windows_ms = std::collections::BTreeSet::new();
            let mut pending = vec![id];
            let mut seen = std::collections::HashSet::new();
            while let Some(producer) = pending.pop() {
                if !seen.insert(producer) {
                    continue;
                }
                for &reader in &readers[producer] {
                    let window = match &configured.node(reader).operator {
                        asap_types::ir::Operator::NonASAP(NonASAPOp::TimeRange {
                            range, ..
                        }) => Some(range.as_millis() as u64),
                        asap_types::ir::Operator::NonASAP(NonASAPOp::TimeShift { .. }) => {
                            pending.push(reader);
                            None
                        }
                        // A chain kept across evaluations reads one pane;
                        // its scans are never certified (retained ancestors).
                        asap_types::ir::Operator::ASAP(ASAPOp::PaneChain {
                            pane_width_ms,
                            pane_count,
                            ..
                        }) => Some(pane_width_ms.saturating_mul(u64::from(*pane_count))),
                        _ => None,
                    };
                    windows_ms.extend(window);
                }
            }
            match windows_ms.len() {
                1 => windows_ms.pop_first(),
                _ => None,
            }
        })
        .collect()
}

/// Whether a summary build's price at its template's parameters is at most
/// its price in any plan that selects it: always, since Stage 1 sizes every
/// summary and Stage 3 only checks, so the template is the size that runs.
fn template_price_is_a_floor(_build: &ASAPOp, _context: &CandidatePlanningContext<'_>) -> bool {
    true
}

fn round_down_small_bounds(costs: &mut [f64]) -> usize {
    let largest = costs.iter().copied().fold(0.0_f64, f64::max);
    let mut removed = 0;
    for cost in costs {
        if *cost > 0.0
            && *cost / largest < crate::search::candidates::dag::MINIMUM_RELATIVE_COEFFICIENT
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
    fn of(configured: &PhysicalCandidatesWithCosts) -> Self {
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
                retention_ms: None,
            });
            result.children.push(children);
            Some(id)
        }
        for (id, config) in configured.physical.candidates.iter().enumerate() {
            let root = if config.timing == ExecutionTiming::IngestionTime
                && config.materialization == Materialization::NotMaterialized
            {
                append(configured.node(id), &mut result)
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
    ) -> Vec<Option<NodeUsage<'a>>> {
        let mut result: Vec<Option<NodeUsage<'a>>> = Vec::with_capacity(self.nodes.len());
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
                node_usage(
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
        configured: &PhysicalCandidatesWithCosts,
        prices: &[Option<NodeUsage<'_>>],
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
                let inputs = &configured.dag.candidates[id].producer_candidates_per_input;
                let agrees = inputs.len() == self.children[root].len()
                    && inputs
                        .iter()
                        .zip(&self.children[root])
                        .all(|(input, &child)| {
                            !input.candidates.is_empty()
                                && input.candidates.iter().all(|supplier| {
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
    configured: &PhysicalCandidatesWithCosts,
    nodes: &'a [Option<PhysicalASAPDAGNode>],
    common_reader_window_ms: &[Option<u64>],
    context: &'a CandidatePlanningContext<'a>,
    query_rate_floor: f64,
    cache: &mut [Option<NodeUsage<'a>>],
    visiting: &mut [bool],
    complete: &mut [bool],
) -> Result<Option<NodeUsage<'a>>, String> {
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
    let result: Result<Option<NodeUsage<'a>>, String> = (|| {
        let mut views = Vec::new();
        for requirement in &configured.dag.candidates[id].producer_candidates_per_input {
            let mut representative = None;
            let mut signature = None;
            for supplier in &requirement.candidates {
                let Some(priced) = certified_price(
                    supplier.0,
                    configured,
                    nodes,
                    common_reader_window_ms,
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
                    asap_types::ir::Operator::ASAP(
                        ASAPOp::PaneChain { family: FieldDataType::Sketch(kind, _), .. }
                        | ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. }
                    ) if matches!(kind.params(), SketchParams::DDSketch { .. })
                )
                .then(|| {
                    matches!(
                        producer.payload,
                        asap_types::ir::Operator::NonASAP(
                            NonASAPOp::Scan { .. } | NonASAPOp::TimeRange { .. }
                        )
                    ) && priced.sources.iter().all(|source| {
                        asap_types::workload::bounds::declared_non_negative(context.data, source)
                    })
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
        // priced at its template's size, its final size
        // ([`template_price_is_a_floor`]). Retention above its own pane count
        // and all whole-plan charges are deliberately omitted.
        Ok(node_usage(
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
            // at least their common width, whatever their offsets.
            asap_cost::trailing_window(common_reader_window_ms[id]),
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
    use crate::search::milp::model::{solve_candidate_proposal, ObservedAssignment};
    use crate::{PlanSelector, PlanningModels};
    use asap_cost::CalibrationFit;
    use asap_logical_asap_optimization::pass1::logical_memo_candidates::generate_per_computation_logical_memo_candidates;
    use asap_logical_asap_optimization::pass1::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::pass2::asap_aware_cse::{
        pane_candidates, window_composition_pass2_candidates,
    };
    use asap_logical_asap_optimization::pass2::memo::candidates::try_populate_pass1;
    use asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups;
    use asap_logical_asap_optimization::test_support::{entry_with, lower_promql};
    use asap_physical_asap_optimization::catalog::materialize_selected_candidates;
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::{OperatorNode, QueryRoot};
    use asap_types::physical::Medium;

    use crate::search::candidates::dag::{InputProducerCandidates, QueryOutputCandidates};

    use asap_types::stages::PhysicalCandidateId;
    use asap_types::workload::*;
    use std::{collections::HashMap, rc::Rc};

    fn fixture() -> (
        PhysicalCandidatesWithCosts,
        Vec<QueryRoot>,
        Vec<QueryWorkloadEntry>,
        DataWorkload,
    ) {
        fixture_for(false)
    }

    fn fixture_for(
        exact: bool,
    ) -> (
        PhysicalCandidatesWithCosts,
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
        PhysicalCandidatesWithCosts,
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
                DagOfLogicalMemoGroups::from_roots(&roots_with_ids),
                &generate_per_computation_candidates(&asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(&roots_with_ids)).unwrap(),
            )
            .unwrap();
            let root_group = memo.query_roots[0].as_operator().unwrap().0 as usize;
            let candidate = memo.candidates[root_group].iter().find(|candidate|
                OperatorNode::reachable(&candidate.root_node).iter().any(|node| if exact {
                    matches!(node.asap(), Some(ASAPOp::SummaryAgg { family: FieldDataType::ExactAggregate(..), child, .. }) if !grouped || matches!(child.non_asap(), Some(NonASAPOp::TimeRange { .. })))
                } else { matches!(node.asap(), Some(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. }) if matches!(kind.params(), SketchParams::DDSketch { .. })) })
            ).unwrap();
            let input = vec![QueryRoot::Operator(Rc::clone(&candidate.root_node))];
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
        let mut configured = PhysicalCandidatesWithCosts::empty_for_tests();
        fn intern(
            node: &Rc<OperatorNode>,
            configured: &mut PhysicalCandidatesWithCosts,
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
            let id = configured.nodes_for_tests().len();
            ids.insert(Rc::as_ptr(node), id);
            let pane = matches!(node.asap(), Some(ASAPOp::PaneChain { .. }));
            let scan = matches!(node.non_asap(), Some(NonASAPOp::Scan { .. }));
            configured.push_for_tests(
                Rc::clone(node),
                if pane || scan {
                    ExecutionTiming::IngestionTime
                } else {
                    ExecutionTiming::QueryTime
                },
                if pane {
                    Materialization::MatAtIngestion(Medium::Memory)
                } else {
                    Materialization::NotMaterialized
                },
                0.0,
                inputs
                    .into_iter()
                    .map(|input| InputProducerCandidates {
                        candidates: vec![PhysicalCandidateId(input)].into(),
                    })
                    .collect(),
            );
            id
        }
        let mut ids = HashMap::new();
        for root in &roots {
            let QueryRoot::Operator(node) = root else {
                unreachable!()
            };
            let id = intern(node, &mut configured, &mut ids);
            configured.dag.query_roots.push(QueryOutputCandidates {
                candidates: vec![PhysicalCandidateId(id)],
            });
            configured.sync_physical_for_tests();
        }
        let chains: Vec<_> = configured
            .nodes_for_tests()
            .iter()
            .enumerate()
            .filter_map(|(id, node)| {
                matches!(node.asap(), Some(ASAPOp::PaneChain { .. }))
                    .then_some(PhysicalCandidateId(id))
            })
            .collect();
        for candidate in &mut configured.dag.candidates {
            for input in &mut candidate.producer_candidates_per_input {
                if chains.contains(&input.candidates[0]) {
                    input.candidates = chains.clone().into();
                }
            }
        }
        configured.sync_physical_for_tests();
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

    fn add_ingestion_transforms(configured: &mut PhysicalCandidatesWithCosts) {
        use asap_types::ir::scalar::{ProjectItem, ScalarExpr};
        use asap_types::ir::TimeRangeKind;
        let chains: Vec<_> = configured
            .nodes_for_tests()
            .iter()
            .enumerate()
            .filter_map(|(id, node)| {
                matches!(node.asap(), Some(ASAPOp::PaneChain { .. })).then_some(id)
            })
            .collect();
        let mut projects = Vec::new();
        for (index, &chain) in chains.iter().enumerate() {
            let scan =
                configured.dag.candidates[chain].producer_candidates_per_input[0].candidates[0];
            configured.sync_physical_for_tests();
            let input = configured.node(scan.0).clone();
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
                let id = PhysicalCandidateId(configured.physical.candidates.len());
                configured.push_for_tests(
                    node,
                    ExecutionTiming::IngestionTime,
                    Materialization::NotMaterialized,
                    0.0,
                    vec![InputProducerCandidates {
                        candidates: vec![producer].into(),
                    }],
                );
                producer = id;
            }
            projects.push(producer);
        }
        for chain in chains {
            configured.dag.candidates[chain].producer_candidates_per_input[0].candidates =
                projects.clone().into();
            configured.sync_physical_for_tests();
            let child = configured.nodes_for_tests()[projects[0].0].clone();
            let template = configured.node(chain);
            configured.set_node_for_tests(
                chain,
                Rc::new(template.with_new_children(|_| child.clone()).unwrap()),
            );
        }
    }

    // Equal row schemas/statistics do not make Scan and Project equivalent
    // DDS pricing inputs: nonnegative raw samples need only one bin domain.
    #[test]
    fn mixed_raw_and_project_inputs_never_overprice_nonnegative_dds() {
        let (mut configured, roots, entries, mut data) = fixture();
        data.sources.get_mut("latency").unwrap().values_non_negative = true;
        let scans: Vec<_> = configured
            .nodes_for_tests()
            .iter()
            .enumerate()
            .filter_map(|(id, node)| {
                matches!(node.non_asap(), Some(NonASAPOp::Scan { .. }))
                    .then_some(PhysicalCandidateId(id))
            })
            .collect();
        let mut projects = Vec::new();
        for scan in &scans {
            let input = configured.node(scan.0).clone();
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
            let id = PhysicalCandidateId(configured.physical.candidates.len());
            configured.push_for_tests(
                project,
                ExecutionTiming::IngestionTime,
                Materialization::NotMaterialized,
                0.0,
                vec![InputProducerCandidates {
                    candidates: vec![*scan].into(),
                }],
            );
            projects.push(id);
        }
        for (id, template) in configured.nodes_for_tests().iter().enumerate() {
            if matches!(template.asap(), Some(ASAPOp::PaneChain { .. })) {
                let input = &mut configured.dag.candidates[id].producer_candidates_per_input[0];
                // Put larger signed-domain Project pricing last to expose the
                // former arbitrary representative selection.
                let mut suppliers = scans.clone();
                suppliers.extend(projects.iter().copied());
                input.candidates = suppliers.into();
            }
        }
        configured.sync_physical_for_tests();
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
            solve_candidate_proposal(&configured.dag, &observations, None).unwrap()
        {
            let selected = proposal.selected;
            let mut modes = vec![None; configured.physical.candidates.len()];
            for id in &selected.selected_candidates {
                modes[id.0] = Some(configured.physical.candidates[id.0].materialization);
            }
            let physical = materialize_selected_candidates(
                &configured.physical,
                &selected,
                &modes,
                &roots,
                &entries,
                &data,
                models.deployment_capabilities.query_time_results_retained,
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
                        .nodes_for_tests()
                        .iter()
                        .enumerate()
                        .filter_map(|(id, node)| {
                            (configured.physical.candidates[id].timing
                                == ExecutionTiming::IngestionTime
                                && matches!(node.non_asap(), Some(NonASAPOp::Project { .. })))
                            .then_some(PhysicalCandidateId(id))
                        })
                        .collect();
                    for project in &projects {
                        configured.dag.candidates[project.0].producer_candidates_per_input[0]
                            .candidates
                            .extend(projects.iter().copied());
                        configured.sync_physical_for_tests();
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
            let mut settings = asap_cost::CostInputs::default();
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
            let calibration = CalibrationFit::from_factor(2.0).unwrap();
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
                    .nodes_for_tests()
                    .iter()
                    .enumerate()
                    .filter_map(|(id, template)| {
                        matches!(
                            template.asap(),
                            Some(ASAPOp::FinalizeExactAccumulator { .. })
                        )
                        .then_some(id)
                    })
                    .collect();
                assert!(!finalizers.is_empty());
                assert!(finalizers
                    .iter()
                    .all(|&id| configured.dag.candidates[id].local_cost > 0.0));
            }
            if transformed {
                assert_eq!(metrics.ingestion_transforms, 4);
            }
            let mut observations = Vec::new();
            let mut valid = 0;
            while let Some(proposal) =
                solve_candidate_proposal(&configured.dag, &observations, None).unwrap()
            {
                let selected = proposal.selected;
                let mut modes = vec![None; configured.physical.candidates.len()];
                for id in &selected.selected_candidates {
                    modes[id.0] = Some(configured.physical.candidates[id.0].materialization);
                }
                let physical = materialize_selected_candidates(
                    &configured.physical,
                    &selected,
                    &modes,
                    &roots,
                    &entries,
                    &data,
                    models.deployment_capabilities.query_time_results_retained,
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
        for root in &configured.dag.query_roots {
            for candidate in &root.candidates {
                configured.physical.candidates[candidate.0].materialization =
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
            .dag
            .candidates
            .iter()
            .zip(&configured.physical.candidates)
        {
            if config.timing == ExecutionTiming::QueryTime {
                assert_eq!(candidate.local_cost, 0.0);
            }
        }
    }
    // Equal row counts do not certify different sources as interchangeable cost inputs.
    #[test]
    fn differing_producer_metadata_leaves_pane_bounds_zero() {
        let (mut configured, roots, entries, mut data) = fixture();
        let scans: Vec<_> = configured
            .nodes_for_tests()
            .iter()
            .enumerate()
            .filter_map(|(id, node)| {
                matches!(node.non_asap(), Some(NonASAPOp::Scan { .. }))
                    .then_some(PhysicalCandidateId(id))
            })
            .collect();
        assert_eq!(scans.len(), 2);
        let original = configured.node(scans[1].0).clone();
        let mut different = (*original).clone();
        let asap_types::ir::Operator::NonASAP(NonASAPOp::Scan { source, .. }) =
            &mut different.operator
        else {
            unreachable!()
        };
        *source = asap_types::ir::operator::Source::TimeSeries {
            metric: "other".into(),
        };
        configured.set_node_for_tests(scans[1].0, Rc::new(different));
        data.sources
            .insert("other".into(), data.sources["latency"].clone());
        let nodes = configured.nodes_for_tests();
        for (candidate, node) in configured.dag.candidates.iter_mut().zip(&nodes) {
            if matches!(node.asap(), Some(ASAPOp::PaneChain { .. })) {
                candidate.producer_candidates_per_input[0].candidates = scans.clone().into();
            }
        }
        configured.sync_physical_for_tests();
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

    /// Each of `queries`' Pass 1 candidates that `pick` selects, sized by
    /// Pass 1 for `target` (their final size), every node at query time and
    /// equal nodes shared: their configured inventory after [`apply`], the
    /// bound of the selected plan, and that plan's evaluated cost.
    fn query_time_bound_and_cost(
        queries: &[&str],
        target: AccuracyTarget,
        settings: &asap_cost::CostInputs,
        pick: fn(&OperatorNode) -> bool,
    ) -> (PhysicalCandidatesWithCosts, f64, f64) {
        let roots_with_ids: Vec<_> = queries
            .iter()
            .enumerate()
            .map(|(id, query)| (id, QueryRoot::Operator(lower_promql(query, target.clone()))))
            .collect();
        let entries: Vec<_> = queries
            .iter()
            .map(|_| {
                entry_with(
                    target.clone(),
                    QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
                        10_000,
                    ))),
                )
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
        let memo = generate_per_computation_logical_memo_candidates(
            DagOfLogicalMemoGroups::from_roots(&roots_with_ids),
            generate_per_computation_candidates(
                &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(
                    &roots_with_ids,
                ),
            )
            .unwrap(),
            &entries,
            &data,
            None,
        )
        .unwrap()
        .memo;
        let picked: Vec<_> = (0..queries.len())
            .map(|query| {
                let root_group = memo.query_roots[query].as_operator().unwrap().0 as usize;
                memo.candidates[root_group]
                    .iter()
                    .find(|candidate| {
                        OperatorNode::reachable(&candidate.root_node)
                            .iter()
                            .any(|node| pick(node))
                    })
                    .unwrap()
            })
            .collect();
        let mut configured = PhysicalCandidatesWithCosts::empty_for_tests();
        fn intern(node: &Rc<OperatorNode>, configured: &mut PhysicalCandidatesWithCosts) -> usize {
            if let Some(id) = configured
                .nodes_for_tests()
                .iter()
                .position(|template| **template == **node)
            {
                return id;
            }
            let mut inputs = Vec::new();
            node.operator.map_children(|child| {
                inputs.push(intern(child, configured));
                Rc::clone(child)
            });
            let id = configured.nodes_for_tests().len();
            configured.push_for_tests(
                Rc::clone(node),
                ExecutionTiming::QueryTime,
                Materialization::NotMaterialized,
                0.0,
                inputs
                    .into_iter()
                    .map(|input| InputProducerCandidates {
                        candidates: vec![PhysicalCandidateId(input)].into(),
                    })
                    .collect(),
            );
            id
        }
        for candidate in &picked {
            let root = intern(&candidate.root_node, &mut configured);
            configured.dag.query_roots.push(QueryOutputCandidates {
                candidates: vec![PhysicalCandidateId(root)],
            });
            configured.sync_physical_for_tests();
        }
        let roots: Vec<_> = picked
            .iter()
            .map(|candidate| QueryRoot::Operator(Rc::clone(&candidate.root_node)))
            .collect();
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
        let selected = solve_candidate_proposal(&configured.dag, &[], None)
            .unwrap()
            .unwrap()
            .selected;
        let modes: Vec<_> = configured
            .physical
            .candidates
            .iter()
            .map(|config| Some(config.materialization))
            .collect();
        let physical = materialize_selected_candidates(
            &configured.physical,
            &selected,
            &modes,
            &roots,
            &entries,
            &data,
            models.deployment_capabilities.query_time_results_retained,
        )
        .unwrap();
        let mut selector = PlanSelector::new(&entries, &data, models, false).unwrap();
        selector.offer(&physical.physical).unwrap();
        let (result, _) = selector.finish().unwrap();
        let cost = result.costs[&result.selected].cost_per_second.unwrap();
        (configured, selected.modeled_cost, cost)
    }

    fn bound_of(
        configured: &PhysicalCandidatesWithCosts,
        matches: impl Fn(&OperatorNode) -> bool,
    ) -> f64 {
        configured
            .nodes_for_tests()
            .iter()
            .zip(&configured.dag.candidates)
            .find(|(template, _)| matches(template))
            .unwrap()
            .1
            .local_cost
    }

    // A query-time scan, its window and the exact build over it are priced
    // from the window's rows at the slowest query rate: positive bounds that
    // never exceed the evaluated plan's cost, also for a shifted window, whose
    // scan Stage 3 prices at the window's width and whose older raw rows it
    // keeps at its own charge.
    #[test]
    fn query_time_scans_and_builds_have_certified_bounds() {
        for query in [
            "sum_over_time(latency[5m])",
            "sum_over_time(latency[5m] offset 1m)",
        ] {
            let (configured, bound, cost) = query_time_bound_and_cost(
                &[query],
                AccuracyTarget::exact(),
                &asap_cost::CostInputs::default(),
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
                "{query}: {scan} {range} {build}"
            );
            assert!(
                bound > 0.0 && bound <= cost * (1.0 + 1e-9),
                "{query}: bound {bound} against cost {cost}"
            );
        }
    }

    // A scan a 15 s and a 5 m window may share reads 5 minutes when the 5 m
    // window reads it. A bound at the 15 s window would price it far below a
    // scan for the 5 m window alone, so it gets no bound (nor do the nodes
    // over it); a scan for one window keeps its bound. Both plans' bounds
    // stay at or below their costs.
    #[test]
    fn a_scan_read_through_different_windows_has_no_bound() {
        let pick: fn(&OperatorNode) -> bool = |node| {
            matches!(node.asap(), Some(ASAPOp::SummaryAgg { family: FieldDataType::ExactAggregate(..), child, .. })
                if matches!(child.non_asap(), Some(NonASAPOp::TimeRange { .. })))
        };
        let is_scan = |node: &OperatorNode| matches!(node.non_asap(), Some(NonASAPOp::Scan { .. }));
        for (queries, scan_bounded) in [
            (&["sum_over_time(latency[5m])"][..], true),
            (
                &["sum_over_time(latency[15s])", "sum_over_time(latency[5m])"][..],
                false,
            ),
        ] {
            let (configured, bound, cost) = query_time_bound_and_cost(
                queries,
                AccuracyTarget::exact(),
                &asap_cost::CostInputs::default(),
                pick,
            );
            let scans = configured
                .nodes_for_tests()
                .iter()
                .filter(|template| is_scan(template))
                .count();
            assert_eq!(scans, 1, "{queries:?} share one scan");
            let scan = bound_of(&configured, is_scan);
            assert_eq!(scan > 0.0, scan_bounded, "{queries:?}: scan bound {scan}");
            assert_eq!(
                bound > 0.0,
                scan_bounded,
                "{queries:?}: build bound {bound}"
            );
            assert!(
                bound <= cost * (1.0 + 1e-9),
                "{queries:?}: bound {bound} against cost {cost}"
            );
        }
    }

    fn is_kll_build(node: &OperatorNode) -> bool {
        matches!(node.asap(), Some(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. })
            if matches!(kind.params(), SketchParams::Kll { .. }))
    }

    // A KLL build is priced at its template's size, which Pass 1 sized for
    // the target and Stage 3 never changes: at every failure probability
    // the bound stays at or below the evaluated cost, and a smaller one
    // gives a larger sketch.
    #[test]
    fn a_kll_build_bound_holds_at_every_failure_probability() {
        let mut costs = Vec::new();
        for failure_probability in [0.01, 0.001, 0.0001] {
            let (configured, bound, cost) = query_time_bound_and_cost(
                &["quantile_over_time(0.9, latency[5m])"],
                AccuracyTarget::max_error(0.01).failure_probability(failure_probability),
                &asap_cost::CostInputs::default(),
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
            "a smaller failure probability sizes a larger sketch: {costs:?}"
        );
    }

    // A measured table need not grow with size, but the template is the
    // size the plan runs: a KLL build with measured entries, and the
    // estimate over it, keep a certified bound at or below the evaluated
    // cost.
    #[test]
    fn a_measured_kll_build_and_estimate_keep_the_bound_below_the_cost() {
        let settings = asap_cost::CostInputs {
            measured_operation_costs: serde_json::from_value(serde_json::json!({"operators": [
            {"operator": "SummaryAgg", "phase": "query_time", "sketch": {"Kll": {"k": 200}}, "ns_per_unit": 300.0, "ns_per_invocation": 200.0},
            {"operator": "SummaryEstimate", "phase": "query_time", "sketch": {"Kll": {"k": 8}}, "ns_per_unit": 0.0, "ns_per_invocation": 5.0e9},
            {"operator": "SummaryEstimate", "phase": "query_time", "sketch": {"Kll": {"k": 20000}}, "ns_per_unit": 0.0, "ns_per_invocation": 1.0}
        ]}))
        .unwrap(),
            ..Default::default()
        };
        for failure_probability in [0.01, 0.0001] {
            let (configured, bound, cost) = query_time_bound_and_cost(
                &["quantile_over_time(0.9, latency[5m])"],
                AccuracyTarget::max_error(0.01).failure_probability(failure_probability),
                &settings,
                is_kll_build,
            );
            assert!(bound_of(&configured, is_kll_build) > 0.0);
            assert!(
                bound > 0.0 && bound <= cost * (1.0 + 1e-9),
                "{failure_probability}: {bound} above {cost}"
            );
        }
    }
}
