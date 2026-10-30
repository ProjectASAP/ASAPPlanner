//! Local cost certification shared by fixed and supplier-dependent profiles.
//!
//! Price each operation from its actual supplier output and state configuration.
//! The fixed-profile adapter admits one interface; supplier_price_options keeps
//! compatible interfaces as joint MILP choices. Ingestion worker counts require
//! certified unary upstream work, never statistics from a template child.
use crate::search::candidates::dag::PhysicalCandidatesWithCosts;
use crate::search::milp::direct_cost::{DirectCostProfile, NodeCostProfile, QueryScheduleRates};
use crate::search::refinement::planner::CandidatePlanningContext;
use crate::{PricedNode, ProducerOutput};
use asap_cost::CostModel;
use asap_types::ir::physical_export::PhysicalASAPDAGNode;
use asap_types::ir::properties::execution_data_state::{DataPrimitive, ExecutionDataState};
use asap_types::ir::properties::{AccuracyScope, ExecutionTiming};
use asap_types::ir::schema::{FieldDataType, SketchParams};
use asap_types::ir::{ASAPOp, NonASAPOp, QueryRoot};
use asap_types::physical::{Materialization, Medium};
use asap_types::stages::PhysicalCandidateId;
use asap_types::workload::{QueryRecurrence, RepeatedDemand};
use std::collections::HashSet;

#[derive(Debug, thiserror::Error)]
pub(in crate::search) enum DirectProfileError {
    #[error(transparent)]
    DeadlineExceeded(#[from] asap_types::stages::CandidateGenerationTimeout),
    #[error("direct cost profile: {0}")]
    Invalid(String),
    #[error("direct cost profile unsupported: {0}")]
    Unsupported(String),
}

pub(in crate::search) struct DirectProfileExtraction {
    pub profile: DirectCostProfile,
    pub supported: Vec<bool>,
    /// The sole input's certified suppliers; empty for scans/unsupported nodes.
    pub suppliers: Vec<Vec<PhysicalCandidateId>>,
}

fn supported_node(node: &asap_types::ir::OperatorNode) -> bool {
    matches!(
        node.non_asap(),
        Some(
            NonASAPOp::Scan { .. }
                | NonASAPOp::TimeRange { .. }
                | NonASAPOp::Filter { .. }
                | NonASAPOp::Project { .. }
                | NonASAPOp::Aggregate { .. }
                | NonASAPOp::BinaryOp { .. }
                | NonASAPOp::Join { .. }
        )
    ) || matches!(
        node.asap(),
        Some(
            ASAPOp::SummaryAgg { .. }
                | ASAPOp::PaneChain { .. }
                | ASAPOp::SummaryEstimate { .. }
                | ASAPOp::FinalizeExactAccumulator { .. }
                | ASAPOp::SummaryMerge { .. }
        )
    )
}

/// Fixed-interface compatibility adapter. Local profiles are priced at one
/// execution; the MILP supplies schedule demand, lifetime and shared charges.
/// Joint extraction additionally carries input tuples and retention tiers.
pub(in crate::search) fn extract(
    configured: &PhysicalCandidatesWithCosts,
    context: &CandidatePlanningContext<'_>,
) -> Result<Option<DirectProfileExtraction>, DirectProfileError> {
    extract_scan_class(configured, context, false)
}

/// A second restricted proposal keeps the newest-pane scan class separate from
/// ordinary full-window scans. The full candidate graph is never changed.
pub(in crate::search) fn extract_latest_panes(
    configured: &PhysicalCandidatesWithCosts,
    context: &CandidatePlanningContext<'_>,
) -> Result<Option<DirectProfileExtraction>, DirectProfileError> {
    extract_scan_class(configured, context, true)
}

struct PreparedProfile {
    nodes: Vec<Option<PhysicalASAPDAGNode>>,
    lookbacks: Vec<Option<u64>>,
    all_lookbacks: Vec<Vec<Option<u64>>>,
    delta: Option<f64>,
    schedules: Option<QueryScheduleRates>,
    rate: f64,
    request_rate: f64,
}

fn prepare_profile(
    configured: &PhysicalCandidatesWithCosts,
    context: &CandidatePlanningContext<'_>,
    prefer_latest_pane: bool,
) -> Result<Option<PreparedProfile>, DirectProfileError> {
    let settings = context.models.plan_selection;
    let CostModel::UsageOverTime(_prices) = &settings.cost_model else {
        return Ok(None);
    };
    let Some(first) = context.entries.first() else {
        return Ok(None);
    };
    if context
        .original_roots
        .iter()
        .any(|root| !matches!(root, QueryRoot::Operator(_)))
        || context.original_roots.len() != context.entries.len()
        || context.original_roots.len() != configured.dag.query_roots.len()
    {
        return Ok(None);
    }
    let equal_fixed = matches!(
        first.recurrence,
        QueryRecurrence::Repeated(
            RepeatedDemand::FixedInterval(_) | RepeatedDemand::FixedIntervalAt { .. }
        )
    ) && context
        .entries
        .iter()
        .all(|entry| entry.recurrence == first.recurrence);
    let has_range = context
        .entries
        .iter()
        .any(|entry| entry.range_evaluation.is_some());
    let rates: Vec<_> = context
        .entries
        .iter()
        .map(|entry| {
            asap_cost::execution_demand::evaluation_rate(
                [(&entry.recurrence, entry.range_evaluation)],
                settings.non_repeating_query_horizon_seconds,
            )
            .map_err(|error| DirectProfileError::Invalid(error.to_string()))
        })
        .collect::<Result<_, _>>()?;
    if rates.iter().any(|rate| !rate.is_finite() || *rate < 0.0) {
        return Ok(None);
    }
    let rate = rates
        .iter()
        .copied()
        .filter(|rate| *rate > 0.0)
        .reduce(f64::max)
        .unwrap_or(1.0);
    let request_rate: f64 = context
        .entries
        .iter()
        .map(|entry| {
            asap_cost::execution_demand::evaluation_rate(
                [(&entry.recurrence, None)],
                settings.non_repeating_query_horizon_seconds,
            )
            .map_err(|error| DirectProfileError::Invalid(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .sum();
    let schedules = if equal_fixed && !has_range {
        None
    } else {
        let general = crate::search::milp::direct_cost::QueryScheduleRates::new(
            context.entries,
            settings.non_repeating_query_horizon_seconds,
            context.deadline,
        )
        .map_err(|error| DirectProfileError::Unsupported(error.to_string()))?;
        Some(general)
    };
    let delta = match failure_budget(context)? {
        Some(delta) => delta,
        None => return Ok(None),
    };
    let count = configured.physical.candidates.len();
    let mut nodes = Vec::with_capacity(count);
    for (id, configuration) in configured.physical.candidates.iter().enumerate() {
        check_deadline(context.deadline)?;
        let node = Some(configured.node(id));
        nodes.push(
            node.filter(|node| {
                supported_node(node)
                    && !(configuration.timing == ExecutionTiming::IngestionTime
                        && configuration.materialization == Materialization::NotMaterialized
                        && configured.dag.query_roots.iter().any(|root| {
                            root.compatible_candidates
                                .contains(&PhysicalCandidateId(id))
                        }))
                    && (requires_source_sizing(node) || stable_size(node, delta))
                    && match configuration.timing {
                        ExecutionTiming::QueryTime => {
                            matches!(
                                configuration.materialization,
                                Materialization::NotMaterialized
                                    | Materialization::MatAtQuery(Medium::Memory)
                            ) || (configuration.materialization
                                == Materialization::MatAtQueryKeptAcrossEvaluations(Medium::Memory)
                                && matches!(node.asap(), Some(ASAPOp::PaneChain { .. }))
                                && context
                                    .models
                                    .deployment_capabilities
                                    .query_time_results_retained
                                && configuration
                                    .may_keep_across_evaluations_per_sole_reader_query
                                    .iter()
                                    .any(|&may_keep| may_keep))
                        }
                        ExecutionTiming::IngestionTime => match node.asap() {
                            Some(ASAPOp::PaneChain { .. }) => {
                                configuration.materialization
                                    == Materialization::MatAtIngestion(Medium::Memory)
                            }
                            _ => {
                                matches!(
                                    node.non_asap(),
                                    Some(
                                        NonASAPOp::Scan { .. }
                                            | NonASAPOp::Filter { .. }
                                            | NonASAPOp::Project { .. }
                                            | NonASAPOp::TimeRange { .. }
                                    )
                                ) && configuration.materialization
                                    == Materialization::NotMaterialized
                            }
                        },
                    }
            })
            .map(|node| {
                let mut position = 0;
                PhysicalASAPDAGNode {
                    id,
                    payload: node.operator.map_children(|_| {
                        let current = position;
                        position += 1;
                        current
                    }),
                    output_state: ExecutionDataState {
                        timing: configuration.timing,
                        primitive: if node.asap().and_then(ASAPOp::produced_state).is_some() {
                            DataPrimitive::SummaryState
                        } else {
                            DataPrimitive::Raw
                        },
                    },
                    materialization: configuration.materialization,
                    output_schema: node.schema.clone(),
                    guarantee: node.guarantee.clone(),
                    retention_ms: None,
                }
            }),
        );
    }
    // Each scan keeps one explicit lookback class, not a maximum requiring an
    // optionally selected witness. Every admitted consumer edge must use it.
    let mut lookbacks = vec![None; count];
    let mut all_lookbacks = vec![vec![None]; count];
    for id in 0..count {
        let Some(scan) = &nodes[id] else { continue };
        if scan.output_state.timing != ExecutionTiming::QueryTime
            || !matches!(
                scan.payload,
                asap_types::ir::Operator::NonASAP(NonASAPOp::Scan { .. })
            )
        {
            continue;
        }
        let mut ordinary = HashSet::new();
        let mut kept = HashSet::new();
        for (consumer, candidate) in configured.dag.candidates.iter().enumerate() {
            check_deadline(context.deadline)?;
            let Some(node) = &nodes[consumer] else {
                continue;
            };
            if node.output_state.timing != ExecutionTiming::QueryTime {
                continue;
            }
            if candidate.producer_requirements.iter().any(|input| {
                input
                    .compatible_producers
                    .contains(&PhysicalCandidateId(id))
            }) {
                if node.materialization
                    == Materialization::MatAtQueryKeptAcrossEvaluations(Medium::Memory)
                {
                    kept.insert(scan_consumer_lookback(node));
                } else {
                    ordinary.insert(scan_consumer_lookback(node));
                }
            }
        }
        let raw_output = configured.dag.query_roots.iter().any(|root| {
            root.compatible_candidates
                .contains(&PhysicalCandidateId(id))
        });
        if !raw_output {
            all_lookbacks[id] = ordinary.union(&kept).copied().collect();
            all_lookbacks[id].sort();
        }
        if raw_output {
            lookbacks[id] = None;
        } else {
            let choices = if prefer_latest_pane && !kept.is_empty() {
                &kept
            } else if !ordinary.is_empty() {
                &ordinary
            } else {
                &kept
            };
            if choices.is_empty() {
                nodes[id] = None;
            } else if choices.contains(&None)
                && !(prefer_latest_pane && choices.iter().any(Option::is_some))
            {
                lookbacks[id] = None;
            } else {
                lookbacks[id] = choices.iter().copied().max().flatten();
            }
        }
    }
    Ok(Some(PreparedProfile {
        nodes,
        lookbacks,
        all_lookbacks,
        delta,
        schedules,
        rate,
        request_rate,
    }))
}

fn extract_scan_class(
    configured: &PhysicalCandidatesWithCosts,
    context: &CandidatePlanningContext<'_>,
    prefer_latest_pane: bool,
) -> Result<Option<DirectProfileExtraction>, DirectProfileError> {
    // The fixed adapter has no selected-reader retention tiers. The joint
    // option extractor carries those decisions instead.
    if context
        .entries
        .iter()
        .any(|entry| entry.range_evaluation != context.entries[0].range_evaluation)
        || context
            .entries
            .iter()
            .any(|entry| entry.range_evaluation.is_some())
            && configured.physical.candidates.iter().any(|c| {
                c.materialization
                    == Materialization::MatAtQueryKeptAcrossEvaluations(Medium::Memory)
            })
    {
        return Ok(None);
    }
    let Some(prepared) = prepare_profile(configured, context, prefer_latest_pane)? else {
        return Ok(None);
    };
    let PreparedProfile {
        nodes,
        lookbacks,
        delta,
        schedules,
        rate,
        request_rate,
        ..
    } = prepared;
    let mut nodes = nodes;
    for (id, node) in nodes.iter_mut().enumerate() {
        if configured.dag.candidates[id].producer_requirements.len() > 1 {
            *node = None;
        }
    }
    let count = nodes.len();
    let settings = context.models.plan_selection;
    let CostModel::UsageOverTime(prices) = &settings.cost_model else {
        return Ok(None);
    };
    let mut cache = vec![None; count];
    let mut visited = vec![false; count];
    let mut visiting = vec![false; count];
    let mut suppliers = vec![Vec::new(); count];
    let mut sketch_counts = vec![0; count];
    let mut raw_sources = vec![None; count];
    let mut closure_work_ms = vec![WorkBounds::default(); count];
    let mut worker_counts = vec![None; count];
    let mut compaction_seconds = vec![0.0; count];
    for id in 0..count {
        price(
            id,
            configured,
            context,
            &nodes,
            &lookbacks,
            &mut cache,
            &mut visited,
            &mut visiting,
            &mut suppliers,
            &mut sketch_counts,
            &mut raw_sources,
            &mut closure_work_ms,
            &mut worker_counts,
            &mut compaction_seconds,
            delta,
            None,
        )?;
    }
    let supported: Vec<_> = cache.iter().map(Option::is_some).collect();

    let zero = || NodeCostProfile {
        per_request: false,
        execution_cost: 0.0,
        work_seconds: 0.0,
        compaction_seconds: 0.0,
        held_memory_cost_per_second: 0.0,
        materialized: false,
        ingestion: false,
        standing_cost_per_second: 0.0,
        raw_retention: None,
    };
    let profiles = cache
        .iter()
        .enumerate()
        .map(|(id, priced)| {
            let Some(priced) = priced else {
                return Ok(zero());
            };
            profile_for_priced(
                priced,
                nodes[id].as_ref().unwrap(),
                compaction_seconds[id],
                context,
            )
        })
        .collect::<Result<Vec<_>, DirectProfileError>>()?;
    let request_ms = settings.measured_operation_costs.ns_per_evaluation * 1e-6;
    Ok(Some(DirectProfileExtraction {
        profile: DirectCostProfile {
            nodes: profiles,
            query_rate: rate,
            schedules,
            request_latency_seconds: request_ms / 1000.0,
            request_cost: request_ms / settings.resource_speed.ms_per_cpu_op
                * prices.cost_per_cpu_op
                * request_rate,
        },
        supported,
        suppliers,
    }))
}

fn profile_for_priced(
    priced: &PricedNode<'_>,
    node: &PhysicalASAPDAGNode,
    compaction_seconds: f64,
    context: &CandidatePlanningContext<'_>,
) -> Result<NodeCostProfile, DirectProfileError> {
    let CostModel::UsageOverTime(prices) = &context.models.plan_selection.cost_model else {
        return Err(DirectProfileError::Unsupported(
            "non-usage objective".into(),
        ));
    };
    let raw_retention = priced
        .raw_scan
        .map(|(source, bytes)| {
            serde_json::to_string(source)
                .map(|identity| (identity, bytes as f64 * prices.cost_per_disk_byte_second))
                .map_err(|error| DirectProfileError::Invalid(error.to_string()))
        })
        .transpose()?;
    Ok(NodeCostProfile {
        per_request: node.materialization
            == Materialization::MatAtQueryKeptAcrossEvaluations(Medium::Memory),
        execution_cost: priced.cpu_ops_per_second * prices.cost_per_cpu_op
            + priced.scanned_bytes_per_second * prices.cost_per_scanned_byte,
        ingestion: node.output_state.timing == ExecutionTiming::IngestionTime,
        standing_cost_per_second: priced.average_memory_bytes * prices.cost_per_memory_byte_second
            + priced.disk_bytes as f64 * prices.cost_per_disk_byte_second,
        work_seconds: priced.work_ms / 1000.0,
        compaction_seconds,
        held_memory_cost_per_second: priced.held.map_or(0.0, |(bytes, _, _)| {
            bytes as f64 * prices.cost_per_memory_byte_second
        }),
        materialized: node.materialization != Materialization::NotMaterialized,
        raw_retention,
    })
}

struct PriceInputs {
    suppliers: Vec<PhysicalCandidateId>,
    upstream_work: WorkBounds,
    upstream_sketches: usize,
    range_span_ms: Option<u64>,
}

#[allow(clippy::too_many_arguments)]
fn price<'a>(
    id: usize,
    configured: &PhysicalCandidatesWithCosts,
    context: &'a CandidatePlanningContext<'_>,
    nodes: &'a [Option<PhysicalASAPDAGNode>],
    lookbacks: &[Option<u64>],
    cache: &mut [Option<PricedNode<'a>>],
    visited: &mut [bool],
    visiting: &mut [bool],
    suppliers: &mut [Vec<PhysicalCandidateId>],
    sketch_counts: &mut [usize],
    raw_sources: &mut [Option<String>],
    closure_work_ms: &mut [WorkBounds],
    worker_counts: &mut [Option<u64>],
    compaction_seconds: &mut [f64],
    delta: Option<f64>,
    input_override: Option<&PriceInputs>,
) -> Result<(), DirectProfileError> {
    check_deadline(context.deadline)?;
    if visited[id] || visiting[id] {
        return Ok(());
    }
    let Some(node) = &nodes[id] else {
        visited[id] = true;
        return Ok(());
    };
    visiting[id] = true;
    let requirements = &configured.dag.candidates[id].producer_requirements;
    let options: Vec<Vec<PhysicalCandidateId>> = input_override.map_or_else(
        || {
            requirements.first().map_or_else(
                || vec![vec![]],
                |input| {
                    input
                        .compatible_producers
                        .iter()
                        .map(|id| vec![*id])
                        .collect()
                },
            )
        },
        |inputs| vec![inputs.suppliers.clone()],
    );
    'bindings: for input_tuple in options {
        let mut views = Vec::new();
        for &supplier in &input_tuple {
            if visiting[supplier.0] {
                continue 'bindings;
            }
            price(
                supplier.0,
                configured,
                context,
                nodes,
                lookbacks,
                cache,
                visited,
                visiting,
                suppliers,
                sketch_counts,
                raw_sources,
                closure_work_ms,
                worker_counts,
                compaction_seconds,
                delta,
                None,
            )?;
            let Some(priced) = &cache[supplier.0] else {
                continue 'bindings;
            };
            let producer = nodes[supplier.0].as_ref().unwrap();
            if producer.output_state.timing == ExecutionTiming::QueryTime
                && matches!(
                    producer.payload,
                    asap_types::ir::Operator::NonASAP(NonASAPOp::Scan { .. })
                )
                && lookbacks[supplier.0] != scan_consumer_lookback(node)
            {
                continue 'bindings;
            }

            let consumer_ingests = node.output_state.timing == ExecutionTiming::IngestionTime;
            let producer_ingests = producer.output_state.timing == ExecutionTiming::IngestionTime;
            if (consumer_ingests && producer.materialization != Materialization::NotMaterialized)
                || (consumer_ingests && !producer_ingests)
                || (!consumer_ingests
                    && producer_ingests
                    && producer.materialization != Materialization::MatAtIngestion(Medium::Memory))
            {
                continue 'bindings;
            }
            views.push(ProducerOutput {
                payload: &producer.payload,
                output_schema: &producer.output_schema,
                output: priced.output,
                sources: priced.sources.clone(),
                family: priced.family,
                unchanged_columns: priced.unchanged_columns.clone(),
            });
        }
        let supplier = input_tuple.first().copied();
        if let asap_types::ir::Operator::ASAP(
            ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(kind, _),
                ..
            }
            | ASAPOp::PaneChain {
                family: FieldDataType::Sketch(kind, _),
                ..
            },
        ) = &node.payload
        {
            if let SketchParams::Hll {
                precision,
                relative_error,
            } = kind.params()
            {
                let Some(source) = supplier.and_then(|supplier| raw_sources[supplier.0].as_deref())
                else {
                    continue;
                };
                let Some(bound) = context
                    .data
                    .sources
                    .get(source)
                    .and_then(|facts| facts.max_distinct_per_evaluation)
                else {
                    continue;
                };
                let Some(delta) = delta else { continue };
                if !hll_size_certified(*precision, *relative_error, delta, bound) {
                    continue;
                }
            }
        }
        let raw_source = match &node.payload {
            asap_types::ir::Operator::NonASAP(NonASAPOp::Scan { source, .. }) => {
                Some(asap_types::workload::bounds::source_name(source).to_string())
            }
            asap_types::ir::Operator::NonASAP(
                NonASAPOp::Filter { .. } | NonASAPOp::TimeRange { .. },
            ) if views.len() == 1 && *views[0].output_schema == node.output_schema => {
                supplier.and_then(|supplier| raw_sources[supplier.0].clone())
            }
            _ => None,
        };
        let sketches = input_override.map_or_else(
            || supplier.map_or(0, |supplier| sketch_counts[supplier.0]),
            |inputs| inputs.upstream_sketches,
        ) + usize::from(matches!(
            &node.payload,
            asap_types::ir::Operator::ASAP(
                ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(..),
                    ..
                } | ASAPOp::PaneChain {
                    family: FieldDataType::Sketch(..),
                    ..
                }
            )
        ));
        // Failure-budget sizing splits delta across approximate inputs. This
        // slice admits at most one sketch build along each unary output path.
        if sketches > 1 && input_override.is_none() {
            continue;
        }
        let (retained_panes, ingestion_window) = match &node.payload {
            asap_types::ir::Operator::ASAP(ASAPOp::PaneChain {
                pane_count,
                pane_width_ms,
                ..
            }) => {
                let window = pane_width_ms.checked_mul(u64::from(*pane_count));
                let span = input_override
                    .and_then(|inputs| inputs.range_span_ms)
                    .unwrap_or_else(|| {
                        context
                            .entries
                            .first()
                            .and_then(|entry| entry.range_evaluation)
                            .map_or(0, |range| range.span_ms)
                    });
                let Some(retention) = window.and_then(|window| window.checked_add(span)) else {
                    continue;
                };
                let retained = (retention / (*pane_width_ms).max(1)).max(u64::from(*pane_count));
                (retained, window)
            }
            _ => (1, None),
        };
        let Ok(priced) = crate::price_node(
            node,
            &views,
            (1, 1.0),
            crate::trailing_window(lookbacks[id]),
            retained_panes,
            ingestion_window,
            context.data,
            context.models.plan_selection,
            context.calibration.factor(),
        ) else {
            continue;
        };
        let prior = input_override.map_or_else(
            || {
                supplier.map_or(WorkBounds::default(), |supplier| {
                    closure_work_ms[supplier.0]
                })
            },
            |inputs| inputs.upstream_work,
        );
        let closure_work = WorkBounds {
            minimum: (prior.minimum + priced.work_ms).next_down().max(0.0),
            maximum: (prior.maximum + priced.work_ms).next_up(),
        };
        if !closure_work.maximum.is_finite() {
            continue;
        }
        let workers = if node.output_state.timing == ExecutionTiming::IngestionTime
            && node.materialization == Materialization::MatAtIngestion(Medium::Memory)
        {
            let Some(workers) = certified_workers(closure_work, nodes.len()) else {
                continue;
            };
            // Same local price does not imply the same upstream worker count.
            // Restrict this producer to one proven bucket of supplier bindings.
            if worker_counts[id].is_some_and(|prior| prior != workers) {
                continue;
            }
            Some(workers)
        } else {
            None
        };
        let compaction = workers.map_or(0.0, |workers| {
            state_compaction_seconds(node, &priced, workers, context)
        });
        if !compaction.is_finite() {
            continue;
        }
        // Compare the actual evaluator outputs, including every piece of the
        // downstream pricing interface. This is not a template-row equivalence.
        if cache[id]
            .as_ref()
            .is_some_and(|prior| !same_price(prior, &priced))
        {
            continue;
        }
        if priced.cost.cost_per_second.is_none()
            || (node.output_state.timing == ExecutionTiming::QueryTime
                && node.materialization
                    != Materialization::MatAtQueryKeptAcrossEvaluations(Medium::Memory)
                && (priced.average_memory_bytes != 0.0
                    || priced.disk_bytes != 0
                    || priced.maintained.is_some()))
        {
            continue;
        }
        if let Some(supplier) = supplier {
            suppliers[id].push(supplier);
        }
        sketch_counts[id] = sketch_counts[id].max(sketches);
        closure_work_ms[id] = if cache[id].is_none() {
            closure_work
        } else {
            WorkBounds {
                minimum: closure_work_ms[id].minimum.min(closure_work.minimum),
                maximum: closure_work_ms[id].maximum.max(closure_work.maximum),
            }
        };
        worker_counts[id] = workers;
        compaction_seconds[id] = compaction;
        if cache[id].is_none() {
            cache[id] = Some(priced);
            raw_sources[id] = raw_source;
        } else if raw_sources[id] != raw_source {
            // The same priced rows need not carry the same raw-population proof.
            // Keep no proof unless every admitted binding preserves it.
            raw_sources[id] = None;
        }
    }
    visiting[id] = false;
    visited[id] = true;
    Ok(())
}

/// A conservative delta valid for every selected unary output path, including
/// rollups whose finer grouping the current evaluator uses for its row bound.
/// Unknown population bounds reject this direct proposal, not the full search.
fn failure_budget(
    context: &CandidatePlanningContext<'_>,
) -> Result<Option<Option<f64>>, DirectProfileError> {
    check_deadline(context.deadline)?;
    let result = (|| {
        let population = || {
            let mut sources = HashSet::new();
            for root in context.original_roots {
                let QueryRoot::Operator(root) = root else {
                    return None;
                };
                for node in asap_types::ir::OperatorNode::reachable(root) {
                    if let Some(NonASAPOp::Scan { source, .. }) = node.non_asap() {
                        sources
                            .insert(asap_types::workload::bounds::source_name(source).to_string());
                    }
                }
            }
            let counts: Option<Vec<_>> = sources
                .iter()
                .map(|source| {
                    context
                        .data
                        .sources
                        .get(source)?
                        .series_count
                        .filter(|count| *count > 0)
                })
                .collect();
            let maximum_source = counts?.into_iter().max()?;
            fn bound(
                node: &std::rc::Rc<asap_types::ir::OperatorNode>,
                maximum_source: u64,
                deadline: Option<std::time::Instant>,
                cache: &mut std::collections::HashMap<usize, u64>,
            ) -> Option<u64> {
                if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
                    return None;
                }
                let identity = std::rc::Rc::as_ptr(node) as usize;
                if let Some(bound) = cache.get(&identity) {
                    return Some(*bound);
                }
                let mut children = Vec::new();
                node.operator.map_children(|child| {
                    children.push(bound(child, maximum_source, deadline, cache))
                });
                let children: Vec<_> = children.into_iter().collect::<Option<_>>()?;
                let value = if children.is_empty() {
                    maximum_source
                } else if children.len() == 1 {
                    children[0]
                } else {
                    // Branching operators can concatenate rows or pair inputs.
                    // Both union and cross-product bounds cover supported joins,
                    // set operations and vector matching; sharing does not reduce
                    // the number of logical input positions in this calculation.
                    let sum = children
                        .iter()
                        .try_fold(0u64, |sum, child| sum.checked_add(*child))?;
                    let product = children
                        .iter()
                        .try_fold(1u64, |product, child| product.checked_mul(*child))?;
                    sum.max(product)
                };
                cache.insert(identity, value);
                Some(value)
            }
            let mut cache = std::collections::HashMap::new();
            context
                .original_roots
                .iter()
                .map(|root| match root {
                    QueryRoot::Operator(node) => {
                        bound(node, maximum_source, context.deadline, &mut cache)
                    }
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()?
                .into_iter()
                .max()
        };
        let mut delta: Option<f64> = None;
        for entry in context.entries {
            let target = &entry.requirements.accuracy;
            if target.is_exact() {
                continue;
            }
            let requested = target
                .failure_probability
                .filter(|delta| delta.is_finite() && *delta > 0.0)?;
            let values = match target.accuracy_scope {
                AccuracyScope::WholeResult => population()?,
                scope => scope.max_values_covered(None)?,
            }
            .max(1);
            let required = requested / values as f64;
            if !required.is_finite() || required <= 0.0 {
                return None;
            }
            delta = Some(delta.map_or(required, |prior| prior.min(required)));
        }
        Some(delta)
    })();
    check_deadline(context.deadline)?;
    Ok(result)
}

fn requires_source_sizing(node: &asap_types::ir::OperatorNode) -> bool {
    matches!(node.asap(), Some(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. } | ASAPOp::PaneChain { family: FieldDataType::Sketch(kind, _), .. })
        if matches!(kind.params(), SketchParams::Hll { .. }))
}

fn hll_size_certified(
    precision: u8,
    relative_error: Option<f64>,
    delta: f64,
    distinct: u64,
) -> bool {
    use asap_accuracy::estimators::hll;
    let error = hll::classic_relative_error(precision, relative_error);
    // Unsupported populations can make min_precision return its input unchanged.
    // The actual confidence certificate is essential, not merely size equality.
    hll::min_precision(error, delta, distinct, precision) == precision
        && hll::classic_failure_probability(precision, error, distinct) <= delta
}

fn stable_size(node: &asap_types::ir::OperatorNode, delta: Option<f64>) -> bool {
    let Some(
        ASAPOp::SummaryAgg {
            family: FieldDataType::Sketch(kind, _),
            ..
        }
        | ASAPOp::PaneChain {
            family: FieldDataType::Sketch(kind, _),
            ..
        },
    ) = node.asap()
    else {
        return true;
    };
    if !matches!(
        kind.params(),
        SketchParams::Kmv {
            relative_error: Some(_),
            ..
        } | SketchParams::Kll { .. }
            | SketchParams::DDSketch { .. }
            | SketchParams::Cms { .. }
            | SketchParams::CountSketch { .. }
    ) {
        return false;
    }
    let Some(delta) = delta else { return false };
    let sized = asap_accuracy::estimators::with_failure_probability(kind.params(), delta);
    match (rmp_serde::to_vec(kind.params()), rmp_serde::to_vec(&sized)) {
        (Ok(before), Ok(after)) => before == after,
        _ => false,
    }
}

fn scan_consumer_lookback(node: &PhysicalASAPDAGNode) -> Option<u64> {
    match &node.payload {
        asap_types::ir::Operator::NonASAP(NonASAPOp::TimeRange { range, .. }) => {
            Some(range.as_millis() as u64)
        }
        asap_types::ir::Operator::ASAP(ASAPOp::PaneChain {
            pane_width_ms,
            pane_count,
            ..
        }) => Some(
            if node.materialization
                == Materialization::MatAtQueryKeptAcrossEvaluations(Medium::Memory)
            {
                *pane_width_ms
            } else {
                pane_width_ms.saturating_mul(u64::from(*pane_count))
            },
        ),
        _ => None,
    }
}

#[derive(Clone, Copy, Default)]
struct WorkBounds {
    minimum: f64,
    maximum: f64,
}

// Outward path sums bound every admitted binding. Account also for the
// evaluator summing those positive values in a different traversal order.
fn certified_workers(work: WorkBounds, node_count: usize) -> Option<u64> {
    let error = node_count as f64 * f64::EPSILON;
    if error >= 0.5 || !work.maximum.is_finite() {
        return None;
    }
    let lower = (work.minimum * (1.0 - error)).next_down().max(0.0);
    let upper = (work.maximum / (1.0 - error)).next_up();
    let first = (lower / 1000.0).ceil().max(1.0);
    let last = (upper / 1000.0).ceil().max(1.0);
    (first == last && last < (1_u64 << 53) as f64).then_some(last as u64)
}

fn state_compaction_seconds(
    node: &PhysicalASAPDAGNode,
    priced: &PricedNode<'_>,
    workers: u64,
    context: &CandidatePlanningContext<'_>,
) -> f64 {
    let settings = context.models.plan_selection;
    let measured = crate::lookup(
        &settings.measured_operation_costs,
        asap_cost::OperatorKind::SummaryMerge,
        ExecutionTiming::QueryTime,
        priced.family,
        |params| crate::sketch_shape(params, crate::StateInputRows::UNKNOWN).1 as f64,
    );
    let merge_ms = measured.map_or(
        settings.resource_speed.ms_per_cpu_op * context.calibration.factor(),
        |unit| (unit.ns_per_unit + unit.ns_per_invocation) * 1e-6,
    );
    let states = priced.output.rows / crate::panes_of(node);
    (workers - 1) as f64 * states as f64 * merge_ms / 1000.0
}

fn check_deadline(deadline: Option<std::time::Instant>) -> Result<(), DirectProfileError> {
    if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
        Err(asap_types::stages::CandidateGenerationTimeout.into())
    } else {
        Ok(())
    }
}

fn same_price(a: &PricedNode<'_>, b: &PricedNode<'_>) -> bool {
    let family = match (a.family, b.family) {
        (crate::StateFamily::None, crate::StateFamily::None) => true,
        (crate::StateFamily::Exact(a), crate::StateFamily::Exact(b)) => a == b,
        (crate::StateFamily::Sketch(a), crate::StateFamily::Sketch(b)) => {
            // SketchParams PartialEq intentionally omits some confidence fields.
            match (rmp_serde::to_vec(a), rmp_serde::to_vec(b)) {
                (Ok(a), Ok(b)) => a == b,
                _ => false,
            }
        }
        _ => false,
    };
    let bits = |p: &PricedNode<'_>| {
        [
            p.work_ms.to_bits(),
            p.cpu_ops_per_second.to_bits(),
            p.scanned_bytes_per_second.to_bits(),
            p.average_memory_bytes.to_bits(),
        ]
    };
    let held = |p: &PricedNode<'_>| {
        p.held
            .map(|(bytes, rate, per_reader)| (bytes, rate.to_bits(), per_reader))
    };
    family
        && a.output == b.output
        && a.sources == b.sources
        && a.unchanged_columns == b.unchanged_columns
        && bits(a) == bits(b)
        && a.peak_memory_bytes == b.peak_memory_bytes
        && a.disk_bytes == b.disk_bytes
        && held(a) == held(b)
        && a.maintained == b.maintained
        && a.raw_scan == b.raw_scan
        && a.cost.cost_per_second.map(f64::to_bits) == b.cost.cost_per_second.map(f64::to_bits)
}

#[cfg(test)]
mod tests {
    #[test]
    fn ingestion_worker_bound_accounts_for_summation_roundoff() {
        let workers = |minimum, maximum| {
            super::certified_workers(super::WorkBounds { minimum, maximum }, 100)
        };
        assert_eq!(workers(999.0, 999.0), Some(1));
        assert_eq!(workers(1000.0, 1000.0), None);
        assert_eq!(workers(1000.0_f64.next_down(), 1000.0), None);
        assert_eq!(workers(1001.0, 1999.0), Some(2));
        assert_eq!(workers(999.0, 1001.0), None);
    }

    #[test]
    fn hll_admission_requires_stable_precision_and_a_supported_population() {
        assert!(super::hll_size_certified(15, Some(0.05), 0.05, 1000));
        assert!(!super::hll_size_certified(12, Some(0.05), 0.05, 1000));
        // The sizing API returns the original precision when no supported
        // precision proves the target. That must not admit an unproved sketch.
        assert!(!super::hll_size_certified(15, Some(0.05), 0.05, 1_000_000));
    }

    use super::*;
    use asap_logical_asap_optimization::test_support::lower_promql;
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::{Operator, OperatorNode, ProjectItem, ScalarExpr};

    use crate::search::candidates::dag::{
        DagOfPhysicalCandidatesWithCosts, PhysicalCandidateWithCost, ProducerRequirement,
        QueryRootCandidates,
    };

    use asap_types::workload::{
        DataWorkload, Predictability, Query, QueryRequirements, QueryWorkloadEntry,
        RepetitionInterval,
    };

    // Direct pricing must not certify a state that Stage 3 would enlarge.
    #[test]
    fn kmv_profiles_require_already_sized_failure_probability() {
        use asap_types::ir::schema::{SketchAlgorithm, SketchKind};
        let mut node = asap_logical_asap_optimization::test_support::cms_summary("count(m)");
        let small = SketchParams::Kmv {
            k: 2,
            relative_error: Some(0.05),
        };
        let sized = asap_accuracy::estimators::with_failure_probability(&small, 0.05);
        let set = |node: &mut OperatorNode, params| {
            let Operator::ASAP(ASAPOp::SummaryAgg { family, .. }) = &mut node.operator else {
                panic!("summary fixture")
            };
            let FieldDataType::Sketch(_, grouping) = family else {
                panic!("sketch fixture")
            };
            *family = FieldDataType::Sketch(
                SketchKind::new(SketchAlgorithm::Kmv, params),
                grouping.clone(),
            );
        };
        set(&mut node, small);
        assert!(!stable_size(&node, Some(0.05)));
        set(&mut node, sized);
        assert!(stable_size(&node, Some(0.05)));
        assert!(!stable_size(&node, Some(0.00001)));
        assert!(!stable_size(&node, None));
    }

    // Equal-priced plain-output consumers retain both exact and sketch paths.
    // The sketch-count bound controls later resizing, not the price interface.
    #[test]
    fn plain_reader_keeps_exact_and_kmv_supplier_interfaces() {
        use asap_types::ir::schema::{SketchAlgorithm, SketchKind, SketchStatistic};
        use std::rc::Rc;
        let target = AccuracyTarget::max_error(0.05)
            .failure_probability(0.05)
            .accuracy_scope(AccuracyScope::PerValue);
        let exact = OperatorNode::reachable(&lower_promql("count(m)", target.clone()))
            .into_iter()
            .find(|node| matches!(node.non_asap(), Some(NonASAPOp::Aggregate { .. })))
            .unwrap();
        let mut summary = asap_logical_asap_optimization::test_support::cms_summary("count(m)");
        let params = asap_accuracy::estimators::with_failure_probability(
            &SketchParams::Kmv {
                k: 2,
                relative_error: Some(0.05),
            },
            0.05,
        );
        let Operator::ASAP(ASAPOp::SummaryAgg { family, .. }) = &mut summary.operator else {
            panic!("summary")
        };
        let FieldDataType::Sketch(_, grouping) = family else {
            panic!("sketch")
        };
        *family = FieldDataType::Sketch(
            SketchKind::new(SketchAlgorithm::Kmv, params),
            grouping.clone(),
        );
        let summary = OperatorNode::new_shared(summary.operator).unwrap();
        let estimate = Rc::new(OperatorNode::with_schema(
            Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input: summary,
                query: SketchStatistic::Cardinality,
            }),
            exact.schema.clone(),
        ));
        let project = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
            cols: exact
                .schema
                .fields
                .iter()
                .enumerate()
                .map(|(index, field)| ProjectItem {
                    alias: Some(field.name.clone()),
                    expr: ScalarExpr::Column(index),
                })
                .collect(),
            qualifier: None,
            child: exact.clone(),
        }))
        .unwrap();
        let mut configured = PhysicalCandidatesWithCosts::empty_for_tests();
        fn append(
            node: Rc<OperatorNode>,
            configured: &mut PhysicalCandidatesWithCosts,
        ) -> PhysicalCandidateId {
            let mut inputs = Vec::new();
            node.operator
                .map_children(|child| inputs.push(append(child.clone(), configured)));
            let id = PhysicalCandidateId(configured.physical.candidates.len());
            configured.push_for_tests(
                node,
                ExecutionTiming::QueryTime,
                Materialization::NotMaterialized,
                0.0,
                inputs
                    .into_iter()
                    .map(|supplier| ProducerRequirement {
                        compatible_producers: vec![supplier].into(),
                    })
                    .collect(),
            );
            id
        }
        let exact_id = append(exact, &mut configured);
        let estimate_id = append(estimate, &mut configured);
        let project_id = append(project.clone(), &mut configured);
        configured.dag.candidates[project_id.0].producer_requirements[0].compatible_producers =
            vec![exact_id, estimate_id].into();
        configured.sync_physical_for_tests();
        configured.dag.query_roots.push(QueryRootCandidates {
            compatible_candidates: vec![project_id],
        });
        configured.sync_physical_for_tests();
        let roots = vec![QueryRoot::Operator(project)];
        let entries = vec![QueryWorkloadEntry {
            query: Query("count(m)".into()),
            requirements: QueryRequirements {
                accuracy: target,
                max_latency_ms: None,
            },
            predictability: Predictability::Predictable { known_at: None },
            recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
                RepetitionInterval(60_000),
            )),
            range_evaluation: None,
        }];
        let data: DataWorkload = serde_json::from_value(serde_json::json!({ "arrival": {"ingesting": {"rows_per_second":100.0}}, "sources":{"m":{"rows_held":1000,"series_count":1}} })).unwrap();
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            seed_separate_plans: false,
            original_roots: &roots,
            entries: &entries,
            data: &data,
            models: crate::PlanningModels::builtin(),
            calibration: asap_cost::CalibrationFit::default(),
        };
        let extracted = extract(&configured, &context).unwrap().unwrap();
        assert!(extracted.supported[exact_id.0] && extracted.supported[estimate_id.0]);
        assert_eq!(
            extracted.suppliers[project_id.0],
            vec![exact_id, estimate_id]
        );
    }

    // Certified KLL/DDS configurations keep their sizes after reconstruction;
    // direct costs include working memory and agree with the complete evaluator.
    #[test]
    fn fixed_sketch_profiles_match_the_real_evaluator() {
        use asap_logical_asap_optimization::pass1::per_computation_candidates::{
            compose_logical_candidate, generate_per_computation_candidates,
            HowToComputeAggregateMeasure,
        };
        use asap_types::ir::schema::SketchAlgorithm;
        use std::rc::Rc;
        fn append(
            node: Rc<OperatorNode>,
            configured: &mut PhysicalCandidatesWithCosts,
        ) -> PhysicalCandidateId {
            let mut inputs = Vec::new();
            node.operator
                .map_children(|child| inputs.push(append(child.clone(), configured)));
            let id = PhysicalCandidateId(configured.physical.candidates.len());
            configured.push_for_tests(
                node,
                ExecutionTiming::QueryTime,
                Materialization::NotMaterialized,
                0.0,
                inputs
                    .into_iter()
                    .map(|supplier| ProducerRequirement {
                        compatible_producers: vec![supplier].into(),
                    })
                    .collect(),
            );
            id
        }
        for algorithm in [
            SketchAlgorithm::Kll,
            SketchAlgorithm::DDSketch,
            SketchAlgorithm::Hll,
        ] {
            for scope in [
                AccuracyScope::PerValue,
                AccuracyScope::WholeResult,
                AccuracyScope::GroupsWithRecordShareAtLeast(0.5),
            ] {
                let target = AccuracyTarget::max_error(0.05)
                    .failure_probability(0.01)
                    .accuracy_scope(scope);
                let query = if algorithm == SketchAlgorithm::Hll {
                    "distinct_over_time(latency[5m])"
                } else {
                    "quantile_over_time(0.95, latency[5m])"
                };
                let original = lower_promql(query, target.clone());
                let generated = generate_per_computation_candidates(vec![(
                    0,
                    QueryRoot::Operator(original.clone()),
                )])
                .unwrap();
                let choice = generated.targets[0].ways_to_compute.iter().position(|candidate|
                    matches!(candidate, HowToComputeAggregateMeasure::Summary(found) if *found == algorithm)).unwrap();
                let roots: Vec<_> = compose_logical_candidate(&generated, &[choice])
                    .unwrap()
                    .into_iter()
                    .map(|(_, root)| root)
                    .collect();
                let data: DataWorkload = serde_json::from_value(serde_json::json!({
                    "arrival":{"ingesting":{"rows_per_second":100.0}},
                    "sources":{"latency":{"series_count":2,"rows_held":30000,"values_non_negative":true,"max_distinct_per_evaluation":1000}}
                })).unwrap();
                // A legal finite configuration is sized before direct pricing.
                let sized = crate::resize_for_failure_probability(
                    &roots,
                    std::slice::from_ref(&target),
                    &data,
                )
                .unwrap();
                let mut configured = PhysicalCandidatesWithCosts::empty_for_tests();
                let QueryRoot::Operator(root) = &sized[0] else {
                    panic!("operator")
                };
                let id = append(root.clone(), &mut configured);
                configured.dag.query_roots.push(QueryRootCandidates {
                    compatible_candidates: vec![id],
                });
                configured.sync_physical_for_tests();
                let originals = vec![QueryRoot::Operator(original)];
                let entries = vec![QueryWorkloadEntry {
                    query: Query(query.into()),
                    requirements: QueryRequirements {
                        accuracy: target,
                        max_latency_ms: None,
                    },
                    predictability: Predictability::Predictable { known_at: None },
                    recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
                        RepetitionInterval(60_000),
                    )),
                    range_evaluation: None,
                }];
                let context = CandidatePlanningContext {
                    deadline: None,
                    refine_materialization: false,
                    seed_separate_plans: false,
                    original_roots: &originals,
                    entries: &entries,
                    data: &data,
                    models: crate::PlanningModels::builtin(),
                    calibration: asap_cost::CalibrationFit::default(),
                };
                let extracted = extract(&configured, &context).unwrap().unwrap();
                assert!(
                    extracted.supported.iter().all(|supported| *supported),
                    "{algorithm:?} {scope:?}"
                );
                let proposal = crate::search::milp::model::solve_direct_cost_proposal_with_metrics(
                    &configured.dag,
                    &[],
                    None,
                    &mut Default::default(),
                    &extracted.profile,
                )
                .unwrap()
                .unwrap();
                let mut rejections =
                    crate::search::refinement::planner::CandidateRejectionSummary::default();
                let evaluated = crate::search::refinement::planner::evaluate_configured_candidate(
                    &configured,
                    &context,
                    &proposal.selected,
                    0,
                    &mut rejections,
                    &mut Vec::new(),
                )
                .unwrap()
                .unwrap_or_else(|| panic!("{algorithm:?} {scope:?}: {rejections:?}"));
                let cost = evaluated.0.costs[&evaluated.0.selected]
                    .cost_per_second
                    .unwrap();
                let direct = proposal.direct_modeled_cost.unwrap();
                assert!(
                    (cost - direct).abs() <= 1e-9 * cost.max(1e-15),
                    "{algorithm:?} {scope:?}: {direct} vs {cost}"
                );
                if scope == AccuracyScope::WholeResult {
                    let mut unknown = data.clone();
                    unknown.sources.get_mut("latency").unwrap().series_count = None;
                    let unproved = CandidatePlanningContext {
                        data: &unknown,
                        ..context
                    };
                    assert!(
                        extract(&configured, &unproved).unwrap().is_none(),
                        "unknown population cannot certify a whole-result sizing budget"
                    );
                }
                let too_small_delta = match algorithm {
                    SketchAlgorithm::Kll => Some(1e-12),
                    _ => None,
                };
                if let Some(delta) = too_small_delta {
                    assert!(configured.nodes_for_tests().iter().any(|node| matches!(
                        node.asap(),
                        Some(ASAPOp::SummaryAgg { .. })
                    ) && !stable_size(
                        node,
                        Some(delta)
                    )));
                }
            }
        }
    }

    #[test]
    fn ingestion_pane_profiles_match_the_real_evaluator() {
        check_pane_profile(Materialization::MatAtIngestion(Medium::Memory));
    }

    #[test]
    fn rebuilt_pane_profiles_match_the_real_evaluator() {
        check_pane_profile(Materialization::NotMaterialized);
        check_pane_profile(Materialization::MatAtQuery(Medium::Memory));
    }

    #[test]
    fn kept_pane_profiles_and_scan_classes_match_the_real_evaluator() {
        check_pane_profile(Materialization::MatAtQueryKeptAcrossEvaluations(
            Medium::Memory,
        ));
    }

    fn check_pane_profile(mode: Materialization) {
        check_pane_profile_with_range(mode, None);
    }

    #[test]
    fn identical_range_pane_profiles_match_the_real_evaluator() {
        let range = Some(asap_types::workload::RangeEvaluation {
            span_ms: 3_600_000,
            step_ms: 60_000,
        });
        for mode in [
            Materialization::NotMaterialized,
            Materialization::MatAtQuery(Medium::Memory),
            Materialization::MatAtIngestion(Medium::Memory),
        ] {
            check_pane_profile_with_range(mode, range);
        }
    }

    fn check_pane_profile_with_range(
        mode: Materialization,
        range_evaluation: Option<asap_types::workload::RangeEvaluation>,
    ) {
        use asap_logical_asap_optimization::pass1::per_computation_candidates::{
            compose_logical_candidate, generate_per_computation_candidates,
            HowToComputeAggregateMeasure,
        };
        use asap_types::ir::schema::SketchAlgorithm;
        use std::rc::Rc;
        fn append(
            node: Rc<OperatorNode>,
            configured: &mut PhysicalCandidatesWithCosts,
            ingestion: bool,
            mode: Materialization,
        ) -> PhysicalCandidateId {
            let ingestion = ingestion
                || (matches!(node.asap(), Some(ASAPOp::PaneChain { .. }))
                    && mode == Materialization::MatAtIngestion(Medium::Memory));
            let pane = matches!(node.asap(), Some(ASAPOp::PaneChain { .. }));
            let mut inputs = Vec::new();
            node.operator.map_children(|child| {
                inputs.push(append(child.clone(), configured, ingestion, mode))
            });
            let id = PhysicalCandidateId(configured.physical.candidates.len());
            configured.push_for_tests(
                node,
                if ingestion {
                    ExecutionTiming::IngestionTime
                } else {
                    ExecutionTiming::QueryTime
                },
                if pane {
                    mode
                } else {
                    Materialization::NotMaterialized
                },
                0.0,
                inputs
                    .into_iter()
                    .map(|supplier| ProducerRequirement {
                        compatible_producers: vec![supplier].into(),
                    })
                    .collect(),
            );
            id
        }
        for (series_count, rows_per_second, speed_multiplier) in
            [(2_u64, 100_u64, 1.0), (100, 1000, 1.0), (2, 100, 1000.0)]
                .into_iter()
                .chain(
                    (mode == Materialization::MatAtIngestion(Medium::Memory)).then_some((
                        2,
                        100,
                        12_345_670.0,
                    )),
                )
        {
            for width in [60_000, 30_000] {
                for algorithm in [
                    SketchAlgorithm::Kll,
                    SketchAlgorithm::DDSketch,
                    SketchAlgorithm::Hll,
                ] {
                    let scope = AccuracyScope::PerValue;
                    let target = AccuracyTarget::max_error(0.05)
                        .failure_probability(0.01)
                        .accuracy_scope(scope);
                    let query = if algorithm == SketchAlgorithm::Hll {
                        "distinct_over_time(latency[5m])"
                    } else {
                        "quantile_over_time(0.95, latency[5m])"
                    };
                    let original = lower_promql(query, target.clone());
                    let generated = generate_per_computation_candidates(vec![(
                        0,
                        QueryRoot::Operator(original.clone()),
                    )])
                    .unwrap();
                    let choice = generated.targets[0].ways_to_compute.iter().position(|candidate|
                    matches!(candidate, HowToComputeAggregateMeasure::Summary(found) if *found == algorithm)).unwrap();
                    let roots: Vec<_> = compose_logical_candidate(&generated, &[choice])
                        .unwrap()
                        .into_iter()
                        .map(|(_, root)| root)
                        .collect();
                    let pane_rules =
                        asap_logical_asap_optimization::pass2::asap_aware_cse::pane_candidates(
                            &roots,
                            &[Some(width)],
                        );
                    let roots = asap_logical_asap_optimization::pass2::asap_aware_cse::window_composition_pass2_candidates(&roots, &pane_rules).unwrap().pop().unwrap().roots.unwrap();
                    let data: DataWorkload = serde_json::from_value(serde_json::json!({
                    "arrival":{"ingesting":{"rows_per_second":rows_per_second}},
                    "sources":{"latency":{"series_count":series_count,"rows_held":rows_per_second * 300,"values_non_negative":true,"max_distinct_per_evaluation":1000}}
                })).unwrap();
                    // A legal finite configuration is sized before direct pricing.
                    let sized = crate::resize_for_failure_probability(
                        &roots,
                        std::slice::from_ref(&target),
                        &data,
                    )
                    .unwrap();
                    let mut configured = PhysicalCandidatesWithCosts::empty_for_tests();
                    let QueryRoot::Operator(root) = &sized[0] else {
                        panic!("operator")
                    };
                    let id = append(root.clone(), &mut configured, false, mode);
                    configured.dag.query_roots.push(QueryRootCandidates {
                        compatible_candidates: vec![id],
                    });
                    configured.sync_physical_for_tests();
                    configured
                        .dag
                        .query_roots
                        .push(configured.dag.query_roots[0].clone());
                    configured.sync_physical_for_tests();
                    let originals = vec![
                        QueryRoot::Operator(original.clone()),
                        QueryRoot::Operator(original),
                    ];
                    let mut entries = vec![QueryWorkloadEntry {
                        query: Query(query.into()),
                        requirements: QueryRequirements {
                            accuracy: target,
                            max_latency_ms: None,
                        },
                        predictability: Predictability::Predictable { known_at: None },
                        recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
                            RepetitionInterval(width as u32),
                        )),
                        range_evaluation,
                    }];
                    entries.push(entries[0].clone());
                    let mut settings = asap_cost::CostInputs::default();
                    if range_evaluation.is_some() {
                        settings.measured_operation_costs.ns_per_evaluation = 1_000_000.0;
                    }
                    settings.resource_speed.ms_per_cpu_op *= speed_multiplier;
                    settings.resource_speed.ms_per_scanned_byte *= speed_multiplier;
                    let deployment = asap_logical_asap_optimization::DeploymentCapabilities {
                        query_time_results_retained: true,
                        ..Default::default()
                    };
                    crate::search::candidates::configurations::fill_may_keep_with_sole_reader(
                        &mut configured,
                        &entries,
                        &data,
                        deployment.query_time_results_retained,
                    );
                    let context = CandidatePlanningContext {
                        deadline: None,
                        refine_materialization: false,
                        seed_separate_plans: false,
                        original_roots: &originals,
                        entries: &entries,
                        data: &data,
                        models: crate::PlanningModels {
                            deployment_capabilities: &deployment,
                            ..crate::PlanningModels::builtin().with_plan_selection(&settings)
                        },
                        calibration: asap_cost::CalibrationFit::default(),
                    };
                    let extracted = extract(&configured, &context).unwrap().unwrap();
                    assert!(
                        extracted.supported.iter().all(|supported| *supported),
                        "{algorithm:?} {scope:?}"
                    );
                    let direct_proposal =
                        crate::search::milp::model::solve_direct_cost_proposal_with_metrics(
                            &configured.dag,
                            &[],
                            None,
                            &mut Default::default(),
                            &extracted.profile,
                        );
                    let proposal = match direct_proposal {
                        Ok(Some(proposal)) => proposal,
                        Err(crate::search::candidates::dag::CandidateSelectionError::UnsupportedDirectCost(_)) if speed_multiplier == 1.0 => {
                            // Default nanosecond work can give sub-threshold
                            // held-memory coefficients. Preserve that fallback.
                            crate::search::milp::model::solve_candidate_proposal(&configured.dag, &[], None).unwrap().unwrap()
                        }
                        result => panic!("{mode:?} {algorithm:?} width={width} series={series_count} speed={speed_multiplier}: {result:?}"),
                    };
                    let mut rejections =
                        crate::search::refinement::planner::CandidateRejectionSummary::default();
                    let evaluated =
                        crate::search::refinement::planner::evaluate_configured_candidate(
                            &configured,
                            &context,
                            &proposal.selected,
                            0,
                            &mut rejections,
                            &mut Vec::new(),
                        )
                        .unwrap()
                        .unwrap_or_else(|| panic!("{algorithm:?} {scope:?}: {rejections:?}"));
                    let cost = evaluated.0.costs[&evaluated.0.selected]
                        .cost_per_second
                        .unwrap();
                    let direct = extracted
                        .profile
                        .selected_cost(&configured.dag, &proposal.selected);
                    assert!(
                        (cost - direct).abs() <= 1e-9 * cost.max(1e-15),
                        "{algorithm:?} {scope:?}: {direct} vs {cost}"
                    );
                    let chain = configured
                        .nodes_for_tests()
                        .iter()
                        .position(|node| matches!(node.asap(), Some(ASAPOp::PaneChain { .. })))
                        .unwrap();
                    let Some(ASAPOp::PaneChain {
                        family, pane_count, ..
                    }) = configured.node(chain).asap()
                    else {
                        unreachable!()
                    };
                    let (_, state_bytes) = crate::summary_shape(
                        family,
                        crate::StateInputRows {
                            rows_per_state: Some(
                                (width * rows_per_second)
                                    .div_ceil(1000)
                                    .div_ceil(series_count),
                            ),
                            non_negative: true,
                        },
                    );
                    let CostModel::UsageOverTime(prices) =
                        &context.models.plan_selection.cost_model
                    else {
                        unreachable!()
                    };
                    let expected_residency = match mode {
                        Materialization::MatAtIngestion(_) => {
                            u64::from(*pane_count)
                                + range_evaluation.map_or(0, |range| range.span_ms / width)
                                + 1
                        }
                        Materialization::MatAtQueryKeptAcrossEvaluations(_) => {
                            u64::from(*pane_count) - 1
                        }
                        _ => 0,
                    } * series_count
                        * state_bytes;
                    let expected_standing =
                        expected_residency as f64 * prices.cost_per_memory_byte_second;
                    assert!(
                        (extracted.profile.nodes[chain].standing_cost_per_second
                            - expected_standing)
                            .abs()
                            <= 1e-12 * expected_standing.max(1e-15)
                    );
                    if matches!(
                        mode,
                        Materialization::MatAtIngestion(_)
                            | Materialization::MatAtQueryKeptAcrossEvaluations(_)
                    ) {
                        assert_eq!(
                            extracted.profile.nodes[chain].held_memory_cost_per_second,
                            0.0
                        );
                    } else {
                        assert!(extracted.profile.nodes[chain].held_memory_cost_per_second > 0.0);
                    }
                    if mode == Materialization::MatAtIngestion(Medium::Memory) {
                        if speed_multiplier > 1e6 {
                            assert!(extracted.profile.nodes[chain].compaction_seconds > 0.0);
                        }
                        let work_ms: f64 = extracted
                            .profile
                            .nodes
                            .iter()
                            .filter(|node| node.ingestion)
                            .map(|node| node.work_seconds * 1000.0)
                            .sum();
                        let mut boundary_settings = settings.clone();
                        boundary_settings.resource_speed.ms_per_cpu_op *= 1000.0 / work_ms;
                        boundary_settings.resource_speed.ms_per_scanned_byte *= 1000.0 / work_ms;
                        let boundary_context = CandidatePlanningContext {
                            models: crate::PlanningModels {
                                plan_selection: &boundary_settings,
                                ..context.models
                            },
                            ..context
                        };
                        assert!(
                            !extract(&configured, &boundary_context)
                                .unwrap()
                                .unwrap()
                                .supported[chain],
                            "a worker threshold cannot certify one fixed bucket"
                        );
                        if speed_multiplier > 1e6 {
                            let FieldDataType::Sketch(kind, _) = family else {
                                unreachable!()
                            };
                            let mut measured = settings.clone();
                            measured.measured_operation_costs.operators.push(
                                asap_cost::measured::OperatorCost {
                                    operator: asap_cost::OperatorKind::SummaryMerge,
                                    phase: Some(ExecutionTiming::QueryTime),
                                    sketch: Some(kind.params().clone()),
                                    exact: None,
                                    measure: None,
                                    ns_per_unit: 2_000_000.0,
                                    ns_per_invocation: 500_000.0,
                                    memory_bytes_per_state: None,
                                },
                            );
                            let measured_context = CandidatePlanningContext {
                                models: crate::PlanningModels {
                                    plan_selection: &measured,
                                    ..context.models
                                },
                                ..context
                            };
                            let profile = extract(&configured, &measured_context)
                                .unwrap()
                                .unwrap()
                                .profile;
                            assert!(profile.nodes[chain].compaction_seconds > 0.0);
                            assert_ne!(
                                profile.nodes[chain].compaction_seconds,
                                extracted.profile.nodes[chain].compaction_seconds
                            );
                            let measured_plan =
                                crate::search::refinement::planner::evaluate_configured_candidate(
                                    &configured,
                                    &measured_context,
                                    &proposal.selected,
                                    2,
                                    &mut Default::default(),
                                    &mut Vec::new(),
                                )
                                .unwrap()
                                .unwrap();
                            let actual = measured_plan.0.costs[&measured_plan.0.selected]
                                .cost_per_second
                                .unwrap();
                            let modeled =
                                profile.selected_cost(&configured.dag, &proposal.selected);
                            assert!(
                                (actual - modeled).abs() <= 1e-9 * actual.max(1e-15),
                                "measured compaction: {actual} vs {modeled}"
                            );
                        }
                    }
                    if mode == Materialization::MatAtQueryKeptAcrossEvaluations(Medium::Memory) {
                        // The same physical scan may feed either a full-window
                        // rebuild or a newest-pane refresh. Keep these priced
                        // domains distinct, and validate each selected binding.
                        let normal_chain =
                            PhysicalCandidateId(configured.physical.candidates.len());
                        let chain_candidate = configured.dag.candidates[chain].clone();
                        let pushed = configured.push_for_tests(
                            configured.node(chain).clone(),
                            ExecutionTiming::QueryTime,
                            Materialization::MatAtQuery(Medium::Memory),
                            chain_candidate.local_cost,
                            chain_candidate.producer_requirements,
                        );
                        assert_eq!(pushed, normal_chain);
                        for candidate in &mut configured.dag.candidates {
                            for input in &mut candidate.producer_requirements {
                                if input
                                    .compatible_producers
                                    .contains(&PhysicalCandidateId(chain))
                                {
                                    input.compatible_producers.push(normal_chain);
                                }
                            }
                        }
                        configured.sync_physical_for_tests();
                        let normal = extract(&configured, &context).unwrap().unwrap();
                        let latest = extract_latest_panes(&configured, &context)
                            .unwrap()
                            .unwrap();
                        assert!(!normal.supported[chain] && normal.supported[normal_chain.0]);
                        assert!(latest.supported[chain] && !latest.supported[normal_chain.0]);
                        let scan = configured
                            .nodes_for_tests()
                            .iter()
                            .position(|node| {
                                matches!(node.non_asap(), Some(NonASAPOp::Scan { .. }))
                            })
                            .unwrap();
                        let ratio = 300_000.0 / width as f64;
                        assert!(
                            (normal.profile.nodes[scan].execution_cost
                                - ratio * latest.profile.nodes[scan].execution_cost)
                                .abs()
                                <= 1e-12 * normal.profile.nodes[scan].execution_cost
                        );
                        let mut full_selection = proposal.selected.clone();
                        for id in &mut full_selection.selected_candidates {
                            if id.0 == chain {
                                *id = normal_chain;
                            }
                        }
                        for edge in &mut full_selection.connections {
                            if edge.producer.0 == chain {
                                edge.producer = normal_chain;
                            }
                            if edge.consumer.0 == chain {
                                edge.consumer = normal_chain;
                            }
                        }
                        let full_evaluated =
                            crate::search::refinement::planner::evaluate_configured_candidate(
                                &configured,
                                &context,
                                &full_selection,
                                1,
                                &mut Default::default(),
                                &mut Vec::new(),
                            )
                            .unwrap()
                            .unwrap();
                        let actual = full_evaluated.0.costs[&full_evaluated.0.selected]
                            .cost_per_second
                            .unwrap();
                        let modeled = normal
                            .profile
                            .selected_cost(&configured.dag, &full_selection);
                        assert!(
                            (actual - modeled).abs() <= 1e-9 * actual.max(1e-15),
                            "full-window class: {actual} vs {modeled}"
                        );
                        let forbidden =
                            asap_logical_asap_optimization::DeploymentCapabilities::default();
                        let no_retention = CandidatePlanningContext {
                            models: crate::PlanningModels {
                                deployment_capabilities: &forbidden,
                                ..context.models
                            },
                            ..context
                        };
                        assert!(
                            !extract(&configured, &no_retention)
                                .unwrap()
                                .unwrap()
                                .supported[chain]
                        );
                    }
                    if algorithm == SketchAlgorithm::Hll {
                        let mut unsupported = data.clone();
                        unsupported
                            .sources
                            .get_mut("latency")
                            .unwrap()
                            .max_distinct_per_evaluation = Some(1_000_000);
                        let outside_certificate = CandidatePlanningContext {
                            data: &unsupported,
                            ..context
                        };
                        assert!(
                            !extract(&configured, &outside_certificate)
                                .unwrap()
                                .unwrap()
                                .supported[chain],
                            "unchanged HLL precision cannot certify an unsupported population"
                        );
                    }
                    if scope == AccuracyScope::WholeResult {
                        let mut unknown = data.clone();
                        unknown.sources.get_mut("latency").unwrap().series_count = None;
                        let unproved = CandidatePlanningContext {
                            data: &unknown,
                            ..context
                        };
                        assert!(
                            extract(&configured, &unproved).unwrap().is_none(),
                            "unknown population cannot certify a whole-result sizing budget"
                        );
                    }
                    let too_small_delta = match algorithm {
                        SketchAlgorithm::Kll => Some(1e-12),
                        _ => None,
                    };
                    if let Some(delta) = too_small_delta {
                        assert!(configured.nodes_for_tests().iter().any(|node| matches!(
                            node.asap(),
                            Some(ASAPOp::PaneChain { .. })
                        ) && !stable_size(
                            node,
                            Some(delta)
                        )));
                    }
                }
            }
        }
    }

    // Real shared KLL/DDS plans verify schedule unions and held-memory lifetime,
    // including two equal frequencies whose different phases cannot coalesce.
    #[test]
    fn heterogeneous_shared_sketch_profiles_match_the_real_evaluator() {
        use asap_logical_asap_optimization::pass1::per_computation_candidates::{
            compose_logical_candidate, generate_per_computation_candidates,
            HowToComputeAggregateMeasure,
        };
        use asap_types::ir::schema::SketchAlgorithm;
        use asap_types::workload::TimestampMs;
        use std::rc::Rc;
        fn append(
            node: Rc<OperatorNode>,
            inventory: &mut PhysicalCandidatesWithCosts,
        ) -> PhysicalCandidateId {
            if let Some(id) = inventory
                .nodes_for_tests()
                .iter()
                .position(|old| Rc::ptr_eq(old, &node))
            {
                return PhysicalCandidateId(id);
            }
            let mut children = Vec::new();
            node.operator
                .map_children(|child| children.push(append(child.clone(), inventory)));
            let materialization = if matches!(node.asap(), Some(ASAPOp::SummaryAgg { .. })) {
                Materialization::MatAtQuery(Medium::Memory)
            } else {
                Materialization::NotMaterialized
            };
            inventory.push_for_tests(
                node,
                ExecutionTiming::QueryTime,
                materialization,
                0.0,
                children
                    .into_iter()
                    .map(|id| ProducerRequirement {
                        compatible_producers: vec![id].into(),
                    })
                    .collect(),
            )
        }
        for algorithm in [SketchAlgorithm::Kll, SketchAlgorithm::DDSketch] {
            for (phases, query_count) in [(false, 2), (true, 2), (false, 5), (true, 5)] {
                let target = AccuracyTarget::max_error(0.05)
                    .failure_probability(0.01)
                    .accuracy_scope(AccuracyScope::PerValue);
                let originals: Vec<_> = [0.95, 0.99, 0.5, 0.7, 0.9]
                    .iter()
                    .take(query_count as usize)
                    .map(|q| {
                        QueryRoot::Operator(lower_promql(
                            &format!("quantile_over_time({q}, latency[5m])"),
                            target.clone(),
                        ))
                    })
                    .collect();
                let generated = generate_per_computation_candidates(
                    originals.iter().cloned().enumerate().collect(),
                )
                .unwrap();
                let choices: Vec<_> = generated.targets.iter().map(|target| target.ways_to_compute.iter().position(|candidate|
                    matches!(candidate,HowToComputeAggregateMeasure::Summary(found) if *found==algorithm)).unwrap()).collect();
                let roots: Vec<_> = compose_logical_candidate(&generated, &choices)
                    .unwrap()
                    .into_iter()
                    .map(|(_, root)| root)
                    .collect();
                // Identical sub-DAGs made one node, as the memo's groups are.
                let shared: Vec<_> =
                    asap_logical_asap_optimization::pass2::cse::share_common_sub_dags(
                        roots
                            .iter()
                            .enumerate()
                            .map(|(id, root)| match root {
                                QueryRoot::Operator(node) => (id, Rc::clone(node)),
                                QueryRoot::Scalar(_) => panic!("operator root"),
                            })
                            .collect(),
                    )
                    .into_iter()
                    .map(|(_, node)| QueryRoot::Operator(node))
                    .collect();
                let mut configured = PhysicalCandidatesWithCosts::empty_for_tests();
                for root in shared {
                    let QueryRoot::Operator(root) = root else {
                        panic!("operator")
                    };
                    let id = append(root, &mut configured);
                    configured.dag.query_roots.push(QueryRootCandidates {
                        compatible_candidates: vec![id],
                    });
                    configured.sync_physical_for_tests();
                }
                assert_eq!(
                    configured
                        .nodes_for_tests()
                        .iter()
                        .filter(|node| matches!(node.asap(), Some(ASAPOp::SummaryAgg { .. })))
                        .count(),
                    1
                );
                let entries: Vec<_> = (0..query_count)
                    .map(|q| QueryWorkloadEntry {
                        query: Query(format!("quantile {q}")),
                        requirements: QueryRequirements {
                            accuracy: target.clone(),
                            max_latency_ms: None,
                        },
                        predictability: Predictability::Predictable { known_at: None },
                        recurrence: QueryRecurrence::Repeated(if phases {
                            RepeatedDemand::FixedIntervalAt {
                                interval: RepetitionInterval(1000),
                                evaluation_phase: TimestampMs(q * 500),
                            }
                        } else {
                            RepeatedDemand::FixedInterval(RepetitionInterval(if q == 0 {
                                500
                            } else {
                                1000
                            }))
                        }),
                        range_evaluation: None,
                    })
                    .collect();
                let data:DataWorkload=serde_json::from_value(serde_json::json!({"arrival":{"ingesting":{"rows_per_second":1.0}},"sources":{"latency":{"series_count":2,"rows_held":300,"values_non_negative":true}}})).unwrap();
                let context = CandidatePlanningContext {
                    deadline: None,
                    refine_materialization: false,
                    seed_separate_plans: false,
                    original_roots: &originals,
                    entries: &entries,
                    data: &data,
                    models: crate::PlanningModels::builtin(),
                    calibration: asap_cost::CalibrationFit::default(),
                };
                let extracted = extract(&configured, &context).unwrap().unwrap();
                assert!(extracted.supported.iter().all(|supported| *supported));
                if query_count == 2 {
                    assert_eq!(
                        extracted
                            .profile
                            .schedules
                            .as_ref()
                            .unwrap()
                            .union_rate([true, true].into_iter()),
                        if phases { 2.0 } else { 3.0 }
                    );
                } else {
                    assert!(extracted.profile.schedules.is_some());
                }
                let proposal = crate::search::milp::model::solve_direct_cost_proposal_with_metrics(
                    &configured.dag,
                    &[],
                    None,
                    &mut Default::default(),
                    &extracted.profile,
                )
                .unwrap()
                .unwrap();
                let mut rejections =
                    crate::search::refinement::planner::CandidateRejectionSummary::default();
                let (selection, _) =
                    crate::search::refinement::planner::evaluate_configured_candidate(
                        &configured,
                        &context,
                        &proposal.selected,
                        0,
                        &mut rejections,
                        &mut Vec::new(),
                    )
                    .unwrap()
                    .unwrap_or_else(|| panic!("{algorithm:?} phases={phases}: {rejections:?}"));
                let actual = selection.costs[&selection.selected]
                    .cost_per_second
                    .unwrap();
                let direct = proposal.direct_modeled_cost.unwrap();
                assert!(
                    (actual - direct).abs() <= 1e-9 * actual.max(1e-15),
                    "{algorithm:?} phases={phases}: {direct} vs {actual}"
                );
            }
        }
    }

    // A template child is not evidence that every supplier has the same rows.
    // Two deliberately different source alternatives exercise cost-interface
    // filtering independently of the logical compatibility check upstream.
    #[test]
    fn different_supplier_statistics_are_not_priced_as_the_template_child() {
        let scan = |metric| {
            OperatorNode::reachable(&lower_promql(metric, AccuracyTarget::exact()))
                .into_iter()
                .find(|node| matches!(node.non_asap(), Some(NonASAPOp::Scan { .. })))
                .unwrap()
        };
        let m = scan("m");
        let n = scan("n");
        let project = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
            cols: m
                .schema
                .fields
                .iter()
                .enumerate()
                .map(|(index, field)| ProjectItem {
                    alias: Some(field.name.clone()),
                    expr: ScalarExpr::Column(index),
                })
                .collect(),
            qualifier: None,
            child: m.clone(),
        }))
        .unwrap();
        let configured = PhysicalCandidatesWithCosts::from_dag_for_tests(
            DagOfPhysicalCandidatesWithCosts {
                candidates: (0..3)
                    .map(|id| PhysicalCandidateWithCost {
                        local_cost: 0.0,
                        producer_requirements: if id == 2 {
                            vec![ProducerRequirement {
                                compatible_producers: vec![
                                    PhysicalCandidateId(0),
                                    PhysicalCandidateId(1),
                                ]
                                .into(),
                            }]
                        } else {
                            vec![]
                        },
                    })
                    .collect(),
                query_roots: vec![QueryRootCandidates {
                    compatible_candidates: vec![PhysicalCandidateId(2)],
                }],
                ..Default::default()
            },
            vec![m, n, project.clone()]
                .into_iter()
                .zip((0..3).map(|_| (ExecutionTiming::QueryTime, Materialization::NotMaterialized)))
                .collect(),
        );
        let roots = vec![QueryRoot::Operator(project)];
        let entries = vec![QueryWorkloadEntry {
            query: Query("m".into()),
            requirements: QueryRequirements {
                accuracy: AccuracyTarget::exact(),
                max_latency_ms: None,
            },
            predictability: Predictability::Predictable { known_at: None },
            recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
                RepetitionInterval(60_000),
            )),
            range_evaluation: None,
        }];
        let data: DataWorkload = serde_json::from_value(serde_json::json!({
            "arrival": {"ingesting": {"rows_per_second": 100.0}},
            "sources": {"m": {"rows_held":10,"series_count":1},"n":{"rows_held":100,"series_count":1}}
        })).unwrap();
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            seed_separate_plans: false,
            original_roots: &roots,
            entries: &entries,
            data: &data,
            models: crate::PlanningModels::builtin(),
            calibration: asap_cost::CalibrationFit::default(),
        };
        let extracted = extract(&configured, &context).unwrap().unwrap();
        assert_eq!(extracted.supported, vec![true, true, true]);
        assert_eq!(extracted.suppliers[2], vec![PhysicalCandidateId(0)]);
        assert!(
            extracted.profile.nodes[1].execution_cost > extracted.profile.nodes[0].execution_cost
        );
        assert_eq!(
            configured.dag.candidates[2].producer_requirements[0]
                .compatible_producers
                .len(),
            2,
            "full catalog remains unchanged"
        );
        let timed = CandidatePlanningContext {
            deadline: Some(std::time::Instant::now()),
            ..context
        };
        assert!(matches!(
            extract(&configured, &timed),
            Err(DirectProfileError::DeadlineExceeded(_))
        ));
    }
}

#[path = "supplier_price_options.rs"]
mod supplier_price_options;
pub(in crate::search) use supplier_price_options::extract_price_options;

#[cfg(test)]
#[path = "direct_price_options_tests.rs"]
mod price_options_tests;

#[cfg(test)]
#[path = "multi_input_price_tests.rs"]
mod multi_input_price_tests;

#[cfg(test)]
#[path = "range_price_options_tests.rs"]
mod range_price_options_tests;
