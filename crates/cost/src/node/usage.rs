//! One node of a physical plan DAG: its resource usage ([`node_usage`])
//! and cost ([`NodeCost`]).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::node::analytical::{
    estimate_operator, AnalyticalCostError, PhysicalOperator, ResourceEstimate,
};
use crate::node::operator_statistics::{
    EdgeStatistics, OperatorStatistics, PartitionStatistics, UnaryEdgeStatistics,
};
use crate::node::state_bytes::{
    keeps_every_value, row_bytes, sketch_shape, summary_shape, StateInputRows,
};
use crate::tumbling_windows_of;
use crate::{aggregate_lookup, lookup, StateFamily};
use crate::{
    CalibrationFit, CostInputs, CostModel, NodeCostSource, OperatorKind, COST_SOURCE,
    MEASURED_COST_SOURCE, MIXED_COST_SOURCE,
};
use asap_types::ir::operator::{GroupKeys, Reduction, Source};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::schema::{FieldDataType, Schema};
use asap_types::ir::{ASAPOp, NonASAPOp, TimeRangeKind};
use asap_types::plan::physical_asap_dag::{
    PhysicalASAPDAG, PhysicalASAPNodeId, PhysicalASAPOperatorPayload as Payload,
};
use asap_types::stages::{Lifetime, StorageMedium};
use asap_types::summary_type::{SketchParams, SketchStatistic};
use asap_types::workload::bounds::{
    declared_non_negative, declared_series, group_count_bound, source_name, source_rows_per_ms,
    unchanged_source_columns,
};
use asap_types::workload::DataWorkload;

/// One node's price per second of wall time, with how it was measured. A
/// node is one operator node of the candidate's physical plan DAG
/// (`PhysicalASAPDAG`), keyed by its [`PhysicalASAPNodeId`] in
/// [`PlanCost::per_node`](crate::PlanCost::per_node); not a machine.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeCost {
    /// Its operator, as the measured table names it.
    pub operator: OperatorKind,
    /// When it runs: ingestion or query time.
    pub timing: ExecutionTiming,
    /// Its price per second under cost model 1; `None` under cost models 2
    /// and 3, which price (or limit) the whole candidate's peaks.
    pub cost_per_second: Option<f64>,
    /// Core-seconds of CPU it takes per second of wall time: its CPU
    /// operations and scanned bytes timed by [`ResourceSpeed`](crate::ResourceSpeed), what a
    /// runtime measures it against. A query's fixed per-request cost
    /// ([`MeasuredOperationCosts::ns_per_evaluation`](crate::MeasuredOperationCosts::ns_per_evaluation)) is counted on the
    /// node the query reads first.
    pub cpu_core_seconds_per_second: f64,
    /// The number of consumers it is computed for, with the
    /// consumers materialized at ingestion time of one refresh window counted once (1
    /// when materialized, [`execution_demand::computations`](crate::plan::execution_demand::computations)).
    pub multiplier: u64,
    /// How it was measured, e.g. "scan 1000 samples; x 1.0000/s; keeps
    /// ... bytes": one computation's work, the times it is computed per
    /// second and what it keeps.
    pub detail: String,
    /// Whether its CPU operations were costed from the measured table or
    /// from the formulas.
    pub cost_source: NodeCostSource,
}

/// The state family of a node with `payload` whose first producer has
/// `producer_family`: a build's own (one sketch instance per group; a
/// shared Hydra structure is not that sketch), inherited by the merges,
/// estimates and finalizations over it, none otherwise.
fn state_family<'a>(
    payload: &'a Payload,
    producer_family: Option<StateFamily<'a>>,
) -> StateFamily<'a> {
    match payload {
        Payload::ASAP(
            ASAPOp::SummaryAgg {
                summary_type: family,
                ..
            }
            | ASAPOp::SummaryTumblingWindowChain {
                summary_type: family,
                ..
            },
        ) => match family {
            FieldDataType::Sketch(..) => {
                todo!("#1207 PR 6: the state family of θ from the candidate's LogicalProperties::parameters")
            }
            FieldDataType::ExactAggregate(kind, _) => StateFamily::Exact(kind),
            _ => StateFamily::None,
        },
        Payload::ASAP(
            ASAPOp::SummaryMerge { .. }
            | ASAPOp::SummaryEstimate { .. }
            | ASAPOp::FinalizeExactAccumulator { .. },
        ) => producer_family.unwrap_or(StateFamily::None),
        _ => StateFamily::None,
    }
}

/// One producer of a node being priced ([`node_usage`]): its payload and
/// schema, its output statistics, the sources it reads and its state
/// family.
#[derive(Debug, Clone)]
pub struct ProducerOutput<'a> {
    pub payload: &'a Payload,
    pub output_schema: &'a Schema,
    pub output: EdgeStatistics,
    pub sources: BTreeSet<&'a str>,
    pub family: StateFamily<'a>,
    /// Its output columns that still hold one source row's label (or
    /// column) values unchanged ([`unchanged_source_columns`]).
    pub unchanged_columns: BTreeSet<&'a str>,
}

/// What one node uses and outputs ([`node_usage`]): its output statistics, sources and state family for its
/// consumers; its work for one computation (one execution) in ms on one
/// core; its usage per second; what is charged
/// once the whole plan is known (memory held for a query's evaluation, an
/// in-memory ingestion window's partial copies, the raw data a query-time
/// scan keeps on disk); its calibration fit and its [`NodeCost`].
#[derive(Debug, Clone)]
pub struct NodeUsage<'a> {
    pub output: EdgeStatistics,
    pub sources: BTreeSet<&'a str>,
    pub family: StateFamily<'a>,
    /// Its output columns that still hold one source row's label (or
    /// column) values unchanged ([`unchanged_source_columns`]).
    pub unchanged_columns: BTreeSet<&'a str>,
    pub work_ms_per_execution: f64,
    pub cpu_ops_per_second: f64,
    pub scanned_bytes_per_second: f64,
    pub average_memory_bytes: f64,
    pub peak_memory_bytes: u64,
    pub disk_bytes: u64,
    /// Query-time memory held while an evaluation runs: (bytes, computations
    /// per second, whether each consumer computes its own copy, held while
    /// that consumer's evaluation runs ([`plan_cost`](crate::plan_cost)), rather than one copy
    /// held until the longest query reading the node finishes).
    pub held: Option<(u64, f64, bool)>,
    /// An in-memory ingestion-time state's bytes per window.
    pub maintained: Option<u64>,
    /// A query-time scan's source and the raw bytes it reads.
    pub raw_scan: Option<(&'a Source, u64)>,
    pub fit: CalibrationFit,
    pub cost: NodeCost,
}

/// Measure and price one node of a physical DAG (one step of
/// [`plan_cost`](crate::plan_cost)): `producers` are its producers in edge order,
/// `rate` its `(multiplier, per_second)` ([`computations`](crate::plan::execution_demand::computations)),
/// `retained_tumbling_window_count` the tumbling windows a tumbling-window chain keeps
/// completed (`retained_tumbling_windows`), and `ingestion_window_ms` the window an
/// ingestion-time node maintains (`maintained_window_ms`).
#[allow(clippy::too_many_arguments)]
pub fn node_usage<'a>(
    node: &'a asap_types::plan::physical_asap_dag::PhysicalASAPDAGNode,
    producers: &[ProducerOutput<'a>],
    rate: (u64, f64),
    retained_tumbling_window_count: u64,
    ingestion_window_ms: Option<u64>,
    data: &DataWorkload,
    settings: &'a CostInputs,
    formula_scale: f64,
) -> Result<NodeUsage<'a>, AnalyticalCostError> {
    let table = &settings.measured_operation_costs;
    let speed = settings.resource_speed;
    let usage_prices = match &settings.cost_model {
        CostModel::UsageOverTime(prices) => Some(prices),
        _ => None,
    };
    let sketch_size =
        |params: &SketchParams| sketch_shape(params, StateInputRows::UNKNOWN).1 as f64;
    let mut fit = CalibrationFit::default();
    let mut held = None;
    let mut maintained = None;
    let mut raw_scan = None;
    // A query-time scan's raw rows retained.
    let mut raw_retained_rows = None;
    let ingestion = node.output_state.timing == ExecutionTiming::IngestionTime;
    let inputs: Vec<EdgeStatistics> = producers.iter().map(|p| p.output).collect();
    let sources: BTreeSet<&str> = match &node.payload {
        Payload::NonASAP(NonASAPOp::Scan { source, .. }) => [source_name(source)].into(),
        _ => producers
            .iter()
            .flat_map(|p| p.sources.iter().copied())
            .collect(),
    };
    // The series its sources declare, when they all do.
    let series = declared_series(&sources, data);
    // The rows per ms its sources receive, which its scans and time
    // ranges read, when the data declares an arrival rate.
    let rows_per_ms = source_rows_per_ms(&sources, data);
    // The fact a rate needs, named when it is missing.
    let missing_rate = |source: &str| AnalyticalCostError::MissingSourceFact {
        source_name: source.to_string(),
        missing: "an arrival rate (the arrival's rows_per_second, or the \
                  source's series_count with sample_interval)",
    };
    let family = state_family(&node.payload, producers.first().map(|p| p.family));
    let unchanged_columns = unchanged_source_columns(
        node,
        &producers
            .iter()
            .map(|p| (p.output_schema, &p.unchanged_columns))
            .collect::<Vec<_>>(),
    );
    let operator = OperatorKind::of(&node.payload);
    let unit_price = match &node.payload {
        Payload::NonASAP(NonASAPOp::Aggregate { measures, .. }) => {
            aggregate_lookup(table, node.output_state.timing, measures)
        }
        _ => lookup(
            table,
            operator,
            node.output_state.timing,
            family,
            sketch_size,
        ),
    };
    // The kernel's measured bytes per state, when the table has them.
    let measured_state_bytes = unit_price
        .and_then(|p| p.memory_bytes_per_state)
        .map(|bytes| bytes.round() as u64);
    let mut cost_source = NodeCostSource::Analytical;
    let input = inputs
        .first()
        .copied()
        .unwrap_or(EdgeStatistics { rows: 1, bytes: 1 });
    let width = row_bytes(&node.output_schema);
    let edge = |rows: u64| -> Result<EdgeStatistics, AnalyticalCostError> {
        Ok(EdgeStatistics {
            rows,
            bytes: rows
                .checked_mul(width)
                .ok_or(AnalyticalCostError::Overflow)?,
        })
    };
    // Whether a summary built here sees raw data samples (its producer a
    // scan or a time range) of sources all declared non-negative.
    let raw_data_samples_non_negative = || {
        let raw = producers.first().is_some_and(|producer| {
            matches!(
                producer.payload,
                Payload::NonASAP(NonASAPOp::Scan { .. } | NonASAPOp::TimeRange { .. })
            )
        });
        raw && sources
            .iter()
            .all(|source| declared_non_negative(data, source))
    };
    let unary = |output| UnaryEdgeStatistics {
        input,
        output,
        promql: None,
    };
    // The schema the node's group and partition keys index.
    let input_schema = producers.first().map(|p| p.output_schema);
    // The rows a grouping reads per evaluation: its input at query
    // time; at ingestion time, the arriving rows of the window it
    // maintains (one second of them does not bound its groups).
    // Query time always has its input rows; an ingestion-time grouping
    // with no window below it is an error (Stage 2 builds none).
    let rows_read = || match ingestion {
        false => Ok(input.rows.max(1)),
        true => {
            let window_ms =
                ingestion_window_ms.ok_or(AnalyticalCostError::IngestionGroupingWithoutWindow)?;
            // Name a source with no rate of its own.
            let rows_per_ms = rows_per_ms.ok_or_else(|| {
                let unrated = sources
                    .iter()
                    .find(|source| source_rows_per_ms(&[**source].into(), data).is_none());
                missing_rate(unrated.copied().unwrap_or("(none)"))
            })?;
            Ok(((rows_per_ms * window_ms as f64).round() as u64).max(1))
        }
    };
    let estimated_groups = |keys: &GroupKeys| {
        let rows_read = rows_read()?;
        Ok(group_count_bound(
            keys,
            input_schema,
            &sources,
            producers
                .first()
                .map_or(&BTreeSet::new(), |producer| &producer.unchanged_columns),
            data,
            rows_read,
        ))
    };
    let groups = |reduction: &Reduction| match reduction {
        Reduction::Reduce(keys) if keys.keys().is_empty() && !keys.is_without() => Ok(1),
        Reduction::Reduce(keys) => estimated_groups(keys),
        // One group per series: the declared series (at query time at
        // most the rows read), else the rows read, each series having at
        // least one.
        Reduction::PerEntity => match (ingestion, series) {
            (true, Some(series)) => Ok(series.max(1)),
            (false, Some(series)) => Ok(series.min(input.rows).max(1)),
            (_, None) => rows_read(),
        },
    };
    let (out, estimate, detail) = match &node.payload {
        Payload::NonASAP(operator) => match operator {
            NonASAPOp::Scan { source, .. } => {
                // At ingestion time, one second of arriving rows; at
                // query time, the arriving rows of the window read above
                // it, at most the rows the source holds (all of them
                // without a window).
                let source = source_name(source);
                let rows_held = data.sources.get(source).and_then(|facts| facts.rows_held);
                let (window_width_ms, retained_lookback_ms): (Option<u64>, Option<u64>) =
                    match ingestion {
                        true => (None, None),
                        false => todo!("#1207 PR 7: a query-time Scan's read per edge (the TimeRange above it) and its retained rows from retention_ms (h_v)"),
                    };
                let lookback = match ingestion {
                    true => Some(1_000),
                    false => window_width_ms,
                };
                let by_rate = |lookback: Option<u64>| {
                    rows_per_ms.zip(lookback).map(|(rows_per_ms, lookback)| {
                        (rows_per_ms * lookback as f64).round() as u64
                    })
                };
                // The raw rows retained for a query-time scan: back to the
                // oldest row it reads, `end_offset + width` (an offset
                // window keeps more than it reads), at most those held.
                if !ingestion {
                    let retained_rows = match (by_rate(retained_lookback_ms), rows_held) {
                        (Some(by_rate), Some(rows_held)) => Some(by_rate.min(rows_held)),
                        (by_rate, rows_held) => by_rate.or(rows_held),
                    };
                    raw_retained_rows = retained_rows.map(|rows| rows.max(1));
                }
                let by_rate = by_rate(lookback);
                let rows = match (ingestion, by_rate, rows_held) {
                    (true, by_rate, _) => by_rate.ok_or_else(|| missing_rate(source))?,
                    (false, Some(by_rate), Some(rows_held)) => by_rate.min(rows_held),
                    (false, by_rate, rows_held) => by_rate.or(rows_held).ok_or_else(|| {
                        AnalyticalCostError::MissingSourceFact {
                            source_name: source.to_string(),
                            missing: match lookback {
                                Some(_) => {
                                    "an arrival rate (the arrival's rows_per_second, or the \
                                     source's series_count with sample_interval) or its \
                                     rows_held"
                                }
                                None => "its rows_held (the scan has no window, so it reads every row held)",
                            },
                        }
                    })?,
                };
                let out = edge(rows.max(1))?;
                let estimate = estimate_operator(
                    PhysicalOperator::Scan,
                    OperatorStatistics::Scan {
                        edges: UnaryEdgeStatistics {
                            input: out,
                            output: out,
                            promql: None,
                        },
                        source_read_bytes: out.bytes,
                    },
                );
                (out, estimate, format!("scan {} samples", out.rows))
            }
            NonASAPOp::Aggregate {
                reduction,
                measures,
                ..
            } => {
                let group_count = groups(reduction)?;
                let keys = match reduction {
                    Reduction::Reduce(keys) => keys.keys().len() as u64,
                    _ => 1,
                };
                let out = edge(group_count)?;
                let held = input.rows.div_ceil(group_count.max(1));
                let estimate = estimate_operator(
                    PhysicalOperator::HashAggregate {
                        grouping_key_count: keys,
                        accumulator_count: measures.len().max(1) as u64,
                    },
                    OperatorStatistics::HashAggregate {
                        edges: unary(out),
                        group_count,
                        key_bytes: keys.checked_mul(16).ok_or(AnalyticalCostError::Overflow)?,
                        accumulator_bytes_per_group: measures
                            .iter()
                            .try_fold(0_u64, |total, measure| {
                                let values = if keeps_every_value(measure) { held } else { 1 };
                                total
                                    .checked_add(
                                        values
                                            .checked_mul(8)
                                            .ok_or(AnalyticalCostError::Overflow)?,
                                    )
                                    .ok_or(AnalyticalCostError::Overflow)
                            })?
                            .max(8),
                    },
                );
                (
                    out,
                    estimate,
                    format!(
                        "hash aggregate {} rows into {group_count} groups",
                        input.rows
                    ),
                )
            }
            NonASAPOp::Sort {
                keys, partition_by, ..
            } => {
                let partitions = if partition_by.keys().is_empty() {
                    1
                } else {
                    estimated_groups(partition_by)?
                };
                let estimate = estimate_operator(
                    PhysicalOperator::InMemoryComparisonSort {
                        ordering_key_count: keys.len() as u64,
                        partitioned: partitions > 1,
                    },
                    OperatorStatistics::InMemoryComparisonSort {
                        edges: unary(input),
                        input_partitioning: split(input, partitions),
                    },
                );
                (
                    input,
                    estimate,
                    format!("sort {} rows in {partitions} partitions", input.rows),
                )
            }
            NonASAPOp::Limit {
                n,
                offset,
                partition_by,
                ..
            } => {
                let partitions = if partition_by.keys().is_empty() {
                    1
                } else {
                    estimated_groups(partition_by)?
                };
                let limit = n.map_or(u64::MAX, |n| (n as u64).saturating_mul(partitions));
                let offset = (*offset as u64).saturating_mul(partitions);
                let out = edge(input.rows.saturating_sub(offset).min(limit))?;
                let estimate = estimate_operator(
                    PhysicalOperator::Limit { limit, offset },
                    OperatorStatistics::Limit { edges: unary(out) },
                );
                (out, estimate, format!("limit to {} rows", out.rows))
            }
            NonASAPOp::TimeShift { .. } => (
                input,
                estimate_operator(
                    PhysicalOperator::TimeShift,
                    OperatorStatistics::PassThrough {
                        edges: unary(input),
                    },
                ),
                format!("time shift: pass {} rows, free", input.rows),
            ),
            NonASAPOp::TimeRange { range, kind, .. } => {
                // A range keeps its window's samples; an instant
                // selector the latest sample of each series.
                // Unknown rate or series: no fewer than its input.
                let retained = match kind {
                    TimeRangeKind::Range => rows_per_ms.map_or(input.rows, |rows_per_ms| {
                        (rows_per_ms * range.as_millis() as f64).round() as u64
                    }),
                    TimeRangeKind::Instant => series.unwrap_or(input.rows),
                };
                let out = edge(input.rows.min(retained.max(1)))?;
                let out = EdgeStatistics {
                    rows: out.rows,
                    bytes: out.bytes.min(input.bytes),
                };
                let estimate = estimate_operator(
                    PhysicalOperator::TimeRange,
                    OperatorStatistics::PassThrough { edges: unary(out) },
                );
                (
                    out,
                    estimate,
                    format!(
                        "time range {range:?}: keep {} of {} rows",
                        out.rows, input.rows
                    ),
                )
            }
            _ => {
                let estimate = estimate_operator(
                    PhysicalOperator::PassThrough,
                    OperatorStatistics::PassThrough {
                        edges: unary(input),
                    },
                );
                (
                    input,
                    estimate,
                    format!("operator: pass {} rows", input.rows),
                )
            }
        },
        Payload::ASAP(ASAPOp::SummaryAgg {
            summary_type: family,
            reduction,
            ..
        }) => {
            let group_count = groups(reduction)?;
            // At query time, each state sees its group's input rows; at
            // ingestion time, the rows of a window one second of
            // arriving rows does not bound.
            let input_rows = StateInputRows {
                rows_per_state: (!ingestion).then(|| input.rows.div_ceil(group_count.max(1))),
                non_negative: raw_data_samples_non_negative(),
            };
            let (depth, state_bytes) = summary_shape(family, input_rows);
            let state_bytes = measured_state_bytes.unwrap_or(state_bytes);
            let out = EdgeStatistics {
                rows: group_count,
                bytes: group_count
                    .checked_mul(state_bytes)
                    .ok_or(AnalyticalCostError::Overflow)?,
            };
            (
                out,
                Ok(ResourceEstimate::new(
                    input.rows as f64 * depth,
                    out.bytes,
                    0,
                )),
                format!(
                    "build {} into {group_count} states: {} rows x depth {depth}",
                    summary_type_name(family),
                    input.rows
                ),
            )
        }
        Payload::ASAP(ASAPOp::SummaryTumblingWindowChain { .. }) => {
            todo!("#1207 PR 7: a chain's states and memory over the windows it retains (h_v)")
        }
        Payload::ASAP(ASAPOp::SummaryMerge { group_by, .. }) => {
            // One operation per input state read; one state per group
            // out. A rollup's groups are those of its keys, at most its
            // input groups.
            let mut group_count = 0;
            for producer in producers {
                if matches!(
                    producer.payload,
                    Payload::ASAP(ASAPOp::SummaryTumblingWindowChain { .. })
                ) {
                    todo!("#1207 PR 7: the states a merge reads from a chain's windows")
                }
                group_count = group_count.max(producer.output.rows);
            }
            let states = merged_states(producers)?;
            if let Some(keys) = group_by {
                group_count = group_count.min(groups(&Reduction::by(keys.clone()))?);
            }
            let out = EdgeStatistics {
                rows: group_count,
                bytes: input
                    .bytes
                    .div_ceil(input.rows.max(1))
                    .checked_mul(group_count)
                    .ok_or(AnalyticalCostError::Overflow)?,
            };
            (
                out,
                Ok(ResourceEstimate::new(states as f64, out.bytes, 0)),
                format!("merge {states} states into {group_count}"),
            )
        }
        Payload::ASAP(ASAPOp::SummaryEstimate { query, .. }) => {
            let per_group = match query {
                SketchStatistic::TopK { k } => *k as u64,
                _ => 1,
            };
            let out = edge(
                input
                    .rows
                    .checked_mul(per_group)
                    .ok_or(AnalyticalCostError::Overflow)?,
            )?;
            (
                out,
                Ok(ResourceEstimate::new(out.rows as f64, 0, 0)),
                format!("estimate {} groups x {per_group}", input.rows),
            )
        }
        Payload::ASAP(ASAPOp::FinalizeExactAccumulator { .. }) => (
            edge(input.rows)?,
            Ok(ResourceEstimate::new(input.rows as f64, 0, 0)),
            format!("finalize {} accumulators", input.rows),
        ),
        _ => (
            edge(input.rows)?,
            Ok(ResourceEstimate::new(input.rows as f64, 0, 0)),
            format!("{} rows", input.rows),
        ),
    };
    let estimate = estimate?;
    let (multiplier, per_second) = rate;
    let formula_ms = estimate.cpu_ops() * speed.ms_per_cpu_op
        + estimate.scan_bytes() as f64 * speed.ms_per_scanned_byte;
    // What one computation processes, the unit its entry is priced per
    // ([`OperatorKind`]): the samples a scan reads, the states a merge
    // reads, otherwise the rows of its (first) input.
    let units = match &node.payload {
        Payload::NonASAP(NonASAPOp::Scan { .. }) => out.rows,
        Payload::ASAP(ASAPOp::SummaryMerge { .. }) => merged_states(producers)?,
        _ => input.rows,
    };
    let mut detail = detail;
    // A node with an entry takes its measured time, scanning included;
    // any other node its formulas' CPU operations times the calibration
    // factor, its scanned bytes unscaled (scaling only the CPU keeps it
    // non-negative whatever the factor).
    let estimate = match unit_price {
        Some(unit) => {
            cost_source = NodeCostSource::Measured;
            let measured_ms = (units as f64 * unit.ns_per_unit + unit.ns_per_invocation) * 1e-6;
            fit.record(measured_ms * per_second, formula_ms * per_second);
            detail += &format!(
                "; measured {units} x {:.3} ns + {:.3} ns",
                unit.ns_per_unit, unit.ns_per_invocation
            );
            ResourceEstimate::new(
                measured_ms / speed.ms_per_cpu_op,
                estimate.peak_memory_bytes(),
                0,
            )
        }
        None if formula_scale != 1.0 => {
            detail += &format!("; formula x {formula_scale:.3} (calibration)");
            ResourceEstimate::new(
                estimate.cpu_ops() * formula_scale,
                estimate.peak_memory_bytes(),
                estimate.scan_bytes(),
            )
        }
        None => estimate,
    };
    let node_ms = estimate.cpu_ops() * speed.ms_per_cpu_op
        + estimate.scan_bytes() as f64 * speed.ms_per_scanned_byte;

    // What this node keeps: memory averaged over time (byte-seconds per
    // second), its peak memory and its disk bytes, added to the
    // candidate's usage below.
    let mut average_memory_bytes = 0.0;
    let mut peak_memory_bytes = 0u64;
    let mut disk_bytes = 0u64;
    let mut detail = format!("{detail}; x {per_second:.4}/s");
    // State retained between computations: at ingestion time, the window
    // being built and the completed ones queries read (a chain of N
    // tumbling windows keeps N + 1), every second. At query time the output is
    // reused only within one evaluation, so it is held only while that
    // evaluation runs (counted below, once latencies are known). The
    // state is what the node holds: its output, or the values an exact
    // aggregate keeps per group when that is more. A computation that
    // keeps nothing holds its working memory while it runs, once per
    // consumer that recomputes it; at query time, that is held for the
    // evaluation too (counted in [`plan_cost`](crate::plan_cost), per consumer: a query's
    // latency, or a materialized consumer's longest query's, as a
    // OneEvaluation state is).
    let state = out.bytes.max(estimate.peak_memory_bytes());
    let (materialized_bytes, medium) = match (ingestion, node.lifetime, &node.payload) {
        (_, Lifetime::None, _) => {
            let working_memory_bytes = estimate.peak_memory_bytes();
            peak_memory_bytes = working_memory_bytes.saturating_mul(multiplier);
            if !ingestion && working_memory_bytes > 0 {
                held = Some((working_memory_bytes, per_second, true));
            }
            (0, StorageMedium::Memory)
        }
        // A tumbling-window chain materialized across evaluations holds the tumbling windows its
        // retention covers ([`tumbling_window_retention_ms`]: N, plus one per tumbling window
        // width of the queries' range-evaluation span), all but the
        // newest between evaluations and all while one runs.
        (
            false,
            Lifetime::AcrossEvaluations(medium),
            Payload::ASAP(ASAPOp::SummaryTumblingWindowChain { .. }),
        ) => {
            let tumbling_windows = tumbling_windows_of(node);
            let retained = retained_tumbling_window_count;
            let between = (state / tumbling_windows)
                .checked_mul(
                    retained
                        .checked_sub(1)
                        .ok_or(AnalyticalCostError::Overflow)?,
                )
                .ok_or(AnalyticalCostError::Overflow)?;
            match medium {
                StorageMedium::Memory => {
                    average_memory_bytes = between as f64;
                    peak_memory_bytes = match retained == tumbling_windows {
                        true => state,
                        false => (state / tumbling_windows)
                            .checked_mul(retained)
                            .ok_or(AnalyticalCostError::Overflow)?,
                    };
                }
                StorageMedium::Disk => disk_bytes = between,
            }
            detail += &format!("; materializes {between} bytes across evaluations");
            (0, medium)
        }
        (_, Lifetime::OneEvaluation(medium), _) => {
            match medium {
                StorageMedium::Memory => {
                    held = Some((state, per_second, false));
                    peak_memory_bytes = state;
                }
                StorageMedium::Disk => disk_bytes = state,
            }
            (0, medium)
        }
        // The retained tumbling windows and the one being built.
        (
            true,
            Lifetime::AcrossEvaluations(medium),
            Payload::ASAP(ASAPOp::SummaryTumblingWindowChain { .. }),
        ) => {
            let tumbling_windows = tumbling_windows_of(node);
            (
                (state / tumbling_windows)
                    .checked_mul(
                        retained_tumbling_window_count
                            .checked_add(1)
                            .ok_or(AnalyticalCostError::Overflow)?,
                    )
                    .ok_or(AnalyticalCostError::Overflow)?,
                medium,
            )
        }
        // A whole state materialized across evaluations: it and the one being built.
        (_, Lifetime::AcrossEvaluations(medium), _) => (
            state.checked_mul(2).ok_or(AnalyticalCostError::Overflow)?,
            medium,
        ),
    };
    if ingestion && node.lifetime == Lifetime::AcrossEvaluations(StorageMedium::Memory) {
        maintained = Some(state / tumbling_windows_of(node));
    }
    if materialized_bytes > 0 {
        match medium {
            StorageMedium::Memory => {
                average_memory_bytes = materialized_bytes as f64;
                peak_memory_bytes = materialized_bytes;
            }
            StorageMedium::Disk => disk_bytes = materialized_bytes,
        }
        detail += &format!("; keeps {materialized_bytes} bytes");
    }
    if let Payload::NonASAP(NonASAPOp::Scan { source, .. }) = &node.payload {
        if !ingestion {
            let retained = match raw_retained_rows {
                Some(rows) => edge(rows)?.bytes,
                None => out.bytes,
            };
            raw_scan = Some((source, retained));
        }
    }
    // One computation's CPU and scan price times its rate, then the
    // materialized state's.
    let cost = usage_prices.map(|prices| {
        (estimate.cpu_ops() * prices.cost_per_cpu_op
            + estimate.scan_bytes() as f64 * prices.cost_per_scanned_byte)
            * per_second
            + prices.cost_per_memory_byte_second * average_memory_bytes
            + prices.cost_per_disk_byte_second * disk_bytes as f64
    });
    if cost.is_some_and(|cost| !cost.is_finite()) {
        return Err(AnalyticalCostError::Overflow);
    }
    Ok(NodeUsage {
        output: out,
        sources,
        family,
        unchanged_columns,
        work_ms_per_execution: node_ms,
        cpu_ops_per_second: estimate.cpu_ops() * per_second,
        scanned_bytes_per_second: estimate.scan_bytes() as f64 * per_second,
        average_memory_bytes,
        peak_memory_bytes,
        disk_bytes,
        held,
        maintained,
        raw_scan,
        fit,
        cost: NodeCost {
            operator,
            timing: node.output_state.timing,
            cost_per_second: cost,
            cpu_core_seconds_per_second: node_ms * per_second / 1_000.0,
            multiplier,
            detail,
            cost_source,
        },
    })
}

/// [`PlanCost::source`] for nodes priced as `per_node` says.
pub(crate) fn cost_source(per_node: &BTreeMap<PhysicalASAPNodeId, NodeCost>) -> &'static str {
    let measured = |node: &NodeCost| node.cost_source == NodeCostSource::Measured;
    if !per_node.values().any(measured) {
        COST_SOURCE
    } else if per_node.values().all(measured) {
        MEASURED_COST_SOURCE
    } else {
        MIXED_COST_SOURCE
    }
}

/// The input states a `SummaryMerge` over `producers` reads: every row of
/// each.
fn merged_states(producers: &[ProducerOutput<'_>]) -> Result<u64, AnalyticalCostError> {
    producers.iter().try_fold(0_u64, |total, producer| {
        total
            .checked_add(producer.output.rows)
            .ok_or(AnalyticalCostError::Overflow)
    })
}

/// The nodes of `dag` by id.
pub fn node_map(dag: &PhysicalASAPDAG) -> NodeMap<'_> {
    dag.nodes.iter().map(|n| (n.id, n)).collect()
}

/// The nodes of a physical DAG by id.
pub type NodeMap<'a> =
    HashMap<PhysicalASAPNodeId, &'a asap_types::plan::physical_asap_dag::PhysicalASAPDAGNode>;

/// A summary type's name in reports: a sketch by its algorithm, e.g.
/// `HydraCms`; an exact accumulator by its kind.
pub fn summary_type_name(summary_type: &FieldDataType) -> String {
    match summary_type {
        FieldDataType::Sketch(algorithm) => format!("{algorithm:?}"),
        FieldDataType::ExactAggregate(kind, _) => format!("exact {kind:?} accumulator"),
        other => format!("{other:?}"),
    }
}

/// `input` split as evenly as integers allow into `partitions` parts.
fn split(input: EdgeStatistics, partitions: u64) -> PartitionStatistics {
    let part = |total: u64, i: u64| total / partitions + u64::from(i < total % partitions);
    PartitionStatistics {
        partitions: (0..partitions)
            .map(|i| EdgeStatistics {
                rows: part(input.rows, i),
                bytes: part(input.bytes, i),
            })
            .collect(),
    }
}
