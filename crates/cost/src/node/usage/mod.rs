//! One node of a physical plan DAG: its resource usage and cost
//! ([`node_resource_usage_and_cost`], [`NodeCost`]).

use std::collections::{BTreeMap, BTreeSet};

use crate::node::analytical::{
    estimate_operator, AnalyticalCostError, PhysicalOperator, ResourceEstimate,
};
use crate::node::operator_statistics::{
    EdgeStatistics, OperatorStatistics, PartitionStatistics, UnaryEdgeStatistics,
};
use crate::node::state_bytes::{
    keeps_every_value, row_bytes, sketch_shape, summary_shape, StateInputRows,
};
use crate::{aggregate_lookup, lookup, StateFamily};
use crate::{
    CalibrationFit, CostInputs, CostModel, NodeCostSource, OperatorKind, COST_SOURCE,
    MEASURED_COST_SOURCE, MIXED_COST_SOURCE,
};
use asap_types::ir::operator::{GroupKeys, Reduction, Source};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::schema::{FieldDataType, Schema};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, TimeRangeKind};
use asap_types::plan::physical_asap_dag::{
    PhysicalASAPNodeId, PhysicalASAPOperatorPayload as Payload,
};
use asap_types::stages::{Lifetime, StorageMedium};
use asap_types::summary_type::{SketchParams, SketchStatistic};
use asap_types::workload::bounds::{
    declared_non_negative, declared_series, group_count_bound, source_name, source_rows_per_ms,
    unchanged_source_columns,
};
use asap_types::workload::DataWorkload;

/// One node's cost per second of wall time, with how it was measured. A
/// node is one operator node of the candidate's physical plan DAG
/// (`PhysicalASAPDAG`), keyed by its [`PhysicalASAPNodeId`] in
/// [`PlanCost::per_node`](crate::PlanCost::per_node); not a machine.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeCost {
    /// Its operator, as the measured table names it.
    pub operator: OperatorKind,
    /// When it runs: ingestion or query time.
    pub timing: ExecutionTiming,
    /// Its cost per second under cost model 1; `None` under cost models 2
    /// and 3, which cost (or limit) the whole candidate's peaks.
    pub cost_per_second: Option<f64>,
    /// Core-seconds of CPU it takes per second of wall time: its CPU
    /// operations and scanned bytes timed by
    /// [`ResourceSpeed`](crate::ResourceSpeed), what a runtime measures it
    /// against. A query's fixed per-request cost
    /// ([`MeasuredOperationCosts::ns_per_evaluation`](crate::MeasuredOperationCosts::ns_per_evaluation))
    /// is counted on the node the query reads first.
    pub cpu_core_seconds_per_second: f64,
    /// The number of consumers it is computed for, with the consumers
    /// materialized at ingestion time of one refresh window counted once
    /// (1 when materialized,
    /// [`execution_demand::computations`](crate::plan::execution_demand::computations)).
    pub multiplier: u64,
    /// How it was measured, e.g. "scan 1000 samples; x 1.0000/s; keeps
    /// ... bytes": one computation's work, the times it is computed per
    /// second and what it keeps.
    pub detail: String,
    /// Whether its CPU operations were costed from the measured table or
    /// from the formulas.
    pub cost_source: NodeCostSource,
}

/// The state family of `node` whose first producer has `producer_family`:
/// a build's own, its θ (`node.parameters`; one sketch instance per group;
/// a shared Hydra structure is not that sketch), inherited by the merges,
/// estimates and finalizations over it, none otherwise. Err
/// [`AnalyticalCostError::UnsupportedCandidate`] for a sketch build without
/// θ, a planner bug.
fn state_family<'a>(
    node: &'a asap_types::plan::physical_asap_dag::PhysicalASAPDAGNode,
    producer_family: Option<StateFamily<'a>>,
) -> Result<StateFamily<'a>, AnalyticalCostError> {
    Ok(match &node.payload {
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
            FieldDataType::Sketch(..) => node
                .parameters
                .as_ref()
                .map(StateFamily::Sketch)
                .ok_or(AnalyticalCostError::UnsupportedCandidate)?,
            FieldDataType::ExactAggregate(kind, _) => StateFamily::Exact(kind),
            _ => StateFamily::None,
        },
        Payload::ASAP(
            ASAPOp::SummaryMerge { .. }
            | ASAPOp::SummaryEstimate { .. }
            | ASAPOp::FinalizeExactAccumulator { .. },
        ) => producer_family.unwrap_or(StateFamily::None),
        _ => StateFamily::None,
    })
}

/// One producer of a node being costed ([`node_resource_usage_and_cost`]): its payload and
/// schema, its output statistics, the sources it reads and its state
/// family.
#[derive(Debug, Clone)]
pub struct ProducerOutput<'a> {
    /// Its payload.
    pub payload: &'a Payload,
    /// Its output schema.
    pub output_schema: &'a Schema,
    /// Its output rows and bytes.
    pub output: EdgeStatistics,
    /// The sources it reads.
    pub sources: BTreeSet<&'a str>,
    /// Its state family (with θ).
    pub family: StateFamily<'a>,
    /// Its output columns that still hold one source row's label (or
    /// column) values unchanged ([`unchanged_source_columns`]).
    pub unchanged_columns: BTreeSet<&'a str>,
    /// The tumbling windows its rows span: those a `TimeRange` selects
    /// from a chain (through `TimeShift`s), 1 for any other node.
    pub tumbling_windows: u64,
    /// The tumbling-window width of the chain whose states its rows are
    /// read from: a chain's own, kept through the `TimeShift`s over it,
    /// so a `TimeRange` over them selects windows; `None` for any other
    /// node.
    pub chain_tumbling_window_width_ms: Option<u64>,
}

/// What one node uses and outputs ([`node_resource_usage_and_cost`]): its
/// output statistics, sources and state family for its consumers; its work
/// for one computation (one execution) in ms on one core; its usage per
/// second (query-time memory held while it computes included); what is
/// charged once the whole plan is known (an in-memory ingestion window's
/// partial copies, the raw data a query-time scan keeps on disk); its
/// calibration fit and its [`NodeCost`].
#[derive(Debug, Clone)]
pub struct NodeResourceUsageAndCost<'a> {
    /// Its output rows and bytes.
    pub output: EdgeStatistics,
    /// The sources it reads.
    pub sources: BTreeSet<&'a str>,
    /// Its state family.
    pub family: StateFamily<'a>,
    /// Its output columns that still hold one source row's label (or
    /// column) values unchanged ([`unchanged_source_columns`]).
    pub unchanged_columns: BTreeSet<&'a str>,
    /// The tumbling windows its rows span: those a `TimeRange` selects
    /// from a chain (through `TimeShift`s), 1 for any other node.
    pub tumbling_windows: u64,
    /// The tumbling-window width of the chain whose states its rows are
    /// read from: a chain's own, kept through the `TimeShift`s over it,
    /// so a `TimeRange` over them selects windows; `None` for any other
    /// node.
    pub chain_tumbling_window_width_ms: Option<u64>,
    /// Work of one execution, in ms on one core.
    pub work_ms_per_execution: f64,
    /// CPU operations per second.
    pub cpu_ops_per_second: f64,
    /// Bytes scanned per second.
    pub scanned_bytes_per_second: f64,
    /// Average memory, including memory a query-time node holds while it
    /// computes.
    pub average_memory_bytes: f64,
    /// Peak memory.
    pub peak_memory_bytes: u64,
    /// Disk bytes.
    pub disk_bytes: u64,
    /// An in-memory ingestion-time state's bytes per window.
    pub maintained: Option<u64>,
    /// A query-time scan's source and the raw bytes it reads.
    pub raw_scan: Option<(&'a Source, u64)>,
    /// Its calibration fit.
    pub fit: CalibrationFit,
    /// Its cost.
    pub cost: NodeCost,
}

impl<'a> NodeResourceUsageAndCost<'a> {
    /// What a consumer reads of the node with `payload` and
    /// `output_schema` that uses this.
    pub fn producer_output(
        &self,
        payload: &'a Payload,
        output_schema: &'a Schema,
    ) -> ProducerOutput<'a> {
        ProducerOutput {
            payload,
            output_schema,
            output: self.output,
            sources: self.sources.clone(),
            family: self.family,
            unchanged_columns: self.unchanged_columns.clone(),
            tumbling_windows: self.tumbling_windows,
            chain_tumbling_window_width_ms: self.chain_tumbling_window_width_ms,
        }
    }
}

/// One second, in ms: an ingestion-time node runs over the rows arriving
/// in one second, and an ingestion-time view that maintains no window is
/// refreshed once per second.
const ONE_SECOND_MS: u64 = 1_000;

/// The window an ingestion-time grouping `node` maintains, in ms: a
/// tumbling-window chain's tumbling window, a `TimeRange`'s range, or the
/// first `TimeRange` or chain below it through filters, projections and
/// time shifts (a view over an ingestion-time chain is refreshed once per
/// chain window; `input_of` gives a node's first input, `operator_of` its
/// operator). `None` when there is none. Over plan nodes ([`plan_cost`])
/// or memo groups (Stage 3, before any plan exists).
///
/// Example: `Aggregate(Sum) <- Filter <- TimeRange(1m) <- Scan`: 60,000;
/// `TimeShift(1h) <- SummaryTumblingWindowChain(1m)`: 60,000.
///
/// [`plan_cost`]: crate::plan_cost
pub fn maintained_window_ms<'o, N: Copy + Ord, C: 'o>(
    node: N,
    operator_of: impl Fn(N) -> &'o Operator<C>,
    input_of: impl Fn(N) -> Option<N>,
) -> Option<u64> {
    let range_ms = |range: &std::time::Duration| u64::try_from(range.as_millis()).ok();
    match operator_of(node) {
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            tumbling_window_width_ms,
            ..
        }) => return Some(*tumbling_window_width_ms),
        Operator::NonASAP(NonASAPOp::TimeRange { range, .. }) => return range_ms(range),
        _ => {}
    }
    let mut seen = BTreeSet::new();
    let mut below = input_of(node)?;
    while seen.insert(below) {
        match operator_of(below) {
            Operator::NonASAP(NonASAPOp::TimeRange { range, .. }) => return range_ms(range),
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                tumbling_window_width_ms,
                ..
            }) => return Some(*tumbling_window_width_ms),
            Operator::NonASAP(
                NonASAPOp::Filter { .. } | NonASAPOp::Project { .. } | NonASAPOp::TimeShift { .. },
            ) => below = input_of(below)?,
            _ => return None,
        }
    }
    None
}

/// Measure and cost one node of a physical DAG (one step of
/// [`plan_cost`]; Stage 3 calls it per candidate): `producers` are its
/// producers in edge order, `rate` its `(multiplier, per_second)`
/// ([`computations`], [`executions_per_second`]), and `window_ms` the time
/// window one execution covers: the window an ingestion-time node
/// maintains ([`maintained_window_ms`]), or a query-time `Scan`'s read, the
/// most offset + range its consumers read ([`read_back_ms`] without a span;
/// `None`: it reads what it retains). A materialized node keeps what its
/// `retention_ms` ($h_v$) covers: a tumbling-window chain its windows in
/// it, a query-time `Scan` and raw rows materialized at ingestion time (a
/// `Scan`, or a `TimeShift` by an offset over one) their raw rows in it,
/// any other view materialized at ingestion time one output per refresh in
/// it (one second of rows when it maintains no window). The formulas' CPU is scaled by
/// `settings.frozen_calibration_factor` (1 when unset).
///
/// [`executions_per_second`]: crate::plan::execution_demand::executions_per_second
pub fn node_resource_usage_and_cost<'a>(
    node: &'a asap_types::plan::physical_asap_dag::PhysicalASAPDAGNode,
    producers: &[ProducerOutput<'a>],
    rate: (u64, f64),
    window_ms: Option<u64>,
    data: &DataWorkload,
    settings: &'a CostInputs,
) -> Result<NodeResourceUsageAndCost<'a>, AnalyticalCostError> {
    // The formulas' CPU scaled by the deployment's calibration, 1 unset.
    let formula_scale = settings.frozen_calibration_factor.unwrap_or(1.0);
    let table = &settings.measured_operation_costs;
    let speed = settings.resource_speed;
    let unit_costs = match &settings.cost_model {
        CostModel::UsageOverTime(unit_cost) => Some(unit_cost),
        _ => None,
    };
    let sketch_size =
        |params: &SketchParams| sketch_shape(params, StateInputRows::UNKNOWN).1 as f64;
    let mut fit = CalibrationFit::default();
    // Query-time memory held while the node computes.
    let mut held_bytes = None;
    let mut maintained = None;
    let mut raw_scan = None;
    // A query-time scan's raw rows retained.
    let mut raw_retained_rows = None;
    let ingestion = node.output_state.timing == ExecutionTiming::IngestionTime;
    // The outputs a node retains: its `retention_ms` over the time between
    // two outputs, at least one. A chain's outputs are its tumbling
    // windows; any other view materialized at ingestion time has one output
    // per refresh (per window it maintains, `window_ms`; per second of
    // arriving rows without one).
    let output_interval_ms = match &node.payload {
        Payload::ASAP(ASAPOp::SummaryTumblingWindowChain {
            tumbling_window_width_ms,
            ..
        }) => *tumbling_window_width_ms,
        _ => window_ms.unwrap_or(ONE_SECOND_MS),
    }
    .max(1);
    // A view materialized across evaluations keeps what its retention
    // covers. Without one (a reader reads it unbounded: a negative offset,
    // an `@` anchor) nothing bounds what it keeps and it cannot be
    // installed, so it is not costed; a `Scan` keeps raw rows (below).
    if node.is_view() && node.retention_ms.is_none() {
        return Err(AnalyticalCostError::MissingOrZero("retention_ms"));
    }
    let retained_output_count = node
        .retention_ms
        .map_or(1, |retention_ms| retention_ms.div_ceil(output_interval_ms))
        .max(1);
    let inputs: Vec<EdgeStatistics> = producers.iter().map(|p| p.output).collect();
    // The tumbling windows its rows span: those a `TimeRange` selects from
    // a chain (through `TimeShift`s, which pass them on), 1 otherwise.
    let tumbling_windows = match &node.payload {
        Payload::NonASAP(NonASAPOp::TimeRange { range, .. })
            if chain_tumbling_window_width_ms(producers).is_some() =>
        {
            selected_tumbling_windows(range, producers)?
        }
        Payload::NonASAP(NonASAPOp::TimeShift { .. }) => producers
            .first()
            .map_or(1, |producer| producer.tumbling_windows),
        _ => 1,
    };
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
    // Raw rows materialized at ingestion time: a `Scan`, or a `TimeShift`
    // by an offset over one, across evaluations. It is a time-indexed store
    // its consumers read back in, retaining `retention_ms` ($h_v$) of rows,
    // which behind an offset includes the offset.
    let raw_rows_materialized_at_ingestion = ingestion
        && matches!(node.lifetime, Lifetime::AcrossEvaluations(_))
        && match &node.payload {
            Payload::NonASAP(NonASAPOp::Scan { .. }) => true,
            Payload::NonASAP(NonASAPOp::TimeShift { shift, .. }) => {
                shift.at.is_none()
                    && producers.first().is_some_and(|producer| {
                        matches!(producer.payload, Payload::NonASAP(NonASAPOp::Scan { .. }))
                    })
            }
            _ => false,
        };
    // The raw rows of its sources back `lookback_ms`: (by arrival rate,
    // when known; the rows they hold, when declared).
    let raw_rows = |lookback_ms: Option<u64>| -> (Option<u64>, Option<u64>) {
        let by_rate = rows_per_ms
            .zip(lookback_ms)
            .map(|(rows_per_ms, lookback_ms)| (rows_per_ms * lookback_ms as f64).round() as u64);
        let rows_held: Option<u64> = sources
            .iter()
            .map(|source| data.sources.get(*source).and_then(|facts| facts.rows_held))
            .sum();
        (by_rate, rows_held)
    };
    // The raw rows of its sources back `lookback_ms` (every row held
    // without one), which a scan reads or keeps: by arrival rate, at most
    // the rows they hold.
    let raw_rows_back = |lookback_ms: Option<u64>| -> Option<u64> {
        match raw_rows(lookback_ms) {
            (Some(by_rate), Some(rows_held)) => Some(by_rate.min(rows_held)),
            (by_rate, rows_held) => by_rate.or(rows_held),
        }
        .map(|rows| rows.max(1))
    };
    // The fact a rate needs, named when it is missing.
    let retained_without_a_bound = |source: &str| AnalyticalCostError::MissingSourceFact {
        source_name: source.to_string(),
        missing: "its rows_held (raw data retained without a bound)",
    };
    let missing_rate = |source: &str| AnalyticalCostError::MissingSourceFact {
        source_name: source.to_string(),
        missing: "an arrival rate (the arrival's rows_per_second, or the \
                  source's series_count with sample_interval)",
    };
    let family = state_family(node, producers.first().map(|p| p.family))?;
    let unchanged_columns = unchanged_source_columns(
        node,
        &producers
            .iter()
            .map(|p| (p.output_schema, &p.unchanged_columns))
            .collect::<Vec<_>>(),
    );
    let operator = OperatorKind::of(&node.payload);
    let measured_unit_cost = match &node.payload {
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
    let measured_state_bytes = measured_unit_cost
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
            let window_ms = window_ms.ok_or(AnalyticalCostError::IngestionGroupingWithoutWindow)?;
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
                // At ingestion, one second of rows; at query time, one
                // execution reads the most its consumers read (`window_ms`),
                // else what the scan retains (its `retention_ms`, $h_v$);
                // `None` reads every row held.
                let lookback = match ingestion {
                    true => Some(ONE_SECOND_MS),
                    false => window_ms.or(node.retention_ms),
                };
                // The raw rows a query-time scan, or an ingestion-time scan
                // materialized across evaluations, keeps: back to the
                // oldest row read, its `retention_ms` ($h_v$; an offset
                // window keeps more than it reads), at most those held.
                if !ingestion || raw_rows_materialized_at_ingestion {
                    raw_retained_rows = raw_rows_back(node.retention_ms);
                }
                let rows = match ingestion {
                    true => raw_rows(lookback).0.ok_or_else(|| missing_rate(source))?,
                    false => raw_rows_back(lookback).ok_or_else(|| {
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
                // Raw data materialized at ingestion time: its consumers
                // read the rows it retains, not one second of them.
                let out = match raw_rows_materialized_at_ingestion {
                    true => edge(raw_retained_rows.ok_or(retained_without_a_bound(source))?)?,
                    false => out,
                };
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
            NonASAPOp::TimeShift { .. } => {
                // Raw rows materialized at ingestion time: like a
                // materialized `Scan`'s, its consumers read the rows it
                // retains (its retention, its offset included), not one
                // second of them.
                let out = match raw_rows_materialized_at_ingestion {
                    true => {
                        raw_retained_rows = raw_rows_back(node.retention_ms);
                        let source = sources.first().copied().unwrap_or("(none)");
                        edge(raw_retained_rows.ok_or(retained_without_a_bound(source))?)?
                    }
                    false => input,
                };
                (
                    out,
                    estimate_operator(
                        PhysicalOperator::TimeShift,
                        OperatorStatistics::PassThrough {
                            edges: unary(input),
                        },
                    ),
                    format!("time shift: pass {} rows, free", input.rows),
                )
            }
            NonASAPOp::TimeRange { range, .. }
                if chain_tumbling_window_width_ms(producers).is_some() =>
            {
                // Over a chain, the windows inside the range: one
                // window's states per window.
                let windows = tumbling_windows;
                let out = EdgeStatistics {
                    rows: input
                        .rows
                        .checked_mul(windows)
                        .ok_or(AnalyticalCostError::Overflow)?,
                    bytes: input
                        .bytes
                        .checked_mul(windows)
                        .ok_or(AnalyticalCostError::Overflow)?,
                };
                // It reads those windows' states from the chain's store.
                let estimate = estimate_operator(
                    PhysicalOperator::TimeRange,
                    OperatorStatistics::PassThrough {
                        edges: UnaryEdgeStatistics {
                            input: out,
                            output: out,
                            promql: None,
                        },
                    },
                );
                (
                    out,
                    estimate,
                    format!("time range {range:?}: {windows} tumbling windows of states"),
                )
            }
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
            let (depth, state_bytes) = summary_shape(family, node.parameters.as_ref(), input_rows)?;
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
        Payload::ASAP(ASAPOp::SummaryTumblingWindowChain {
            summary_type: family,
            reduction,
            tumbling_window_width_ms,
            ..
        }) => {
            // One window's states: each input row updates the window its
            // time falls in. At ingestion time the rows of one window
            // arrive at the rate of one second's `input.rows`; at query
            // time its input holds the rows of every retained window.
            let group_count = groups(reduction)?;
            let rows_per_tumbling_window = match ingestion {
                true => u64::try_from(
                    (u128::from(input.rows) * u128::from(*tumbling_window_width_ms))
                        .div_ceil(u128::from(ONE_SECOND_MS)),
                )
                .map_err(|_| AnalyticalCostError::Overflow)?,
                false => input.rows.div_ceil(retained_output_count),
            };
            let input_rows = StateInputRows {
                rows_per_state: Some(rows_per_tumbling_window.div_ceil(group_count.max(1))),
                non_negative: raw_data_samples_non_negative(),
            };
            let (depth, state_bytes) = summary_shape(family, node.parameters.as_ref(), input_rows)?;
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
                    input.rows as f64 * (depth + 1.0),
                    out.bytes,
                    0,
                )),
                format!(
                    "build {} in tumbling windows of {tumbling_window_width_ms} ms for \
                     {group_count} groups: {} rows x (1 + depth {depth})",
                    summary_type_name(family),
                    input.rows
                ),
            )
        }
        Payload::ASAP(ASAPOp::SummaryMerge { group_by, .. }) => {
            // One operation per input state read; one state per group
            // out: an input's groups are its states of one window (all
            // its states unless it selects a chain's windows). A
            // rollup's groups are those of its keys, at most its input
            // groups.
            let mut group_count = 0;
            for producer in producers {
                group_count = group_count.max(
                    producer
                        .output
                        .rows
                        .div_ceil(producer.tumbling_windows.max(1)),
                );
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
    // What one computation processes, the unit its entry is costed per
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
    let estimate = match measured_unit_cost {
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
    // State retained between computations: at ingestion time, the output
    // being built and the completed ones its retention covers (a view of N
    // retained outputs keeps N + 1), every second. At query time the
    // output is held only while the node computes (below). The state is
    // what the node holds: its output, or the values an exact aggregate
    // keeps per group when that is more. A computation that keeps nothing holds its
    // working memory while it runs.
    let state = out.bytes.max(estimate.peak_memory_bytes());
    let (materialized_bytes, medium) = match (ingestion, node.lifetime, &node.payload) {
        (_, Lifetime::None, _) => {
            let working_memory_bytes = estimate.peak_memory_bytes();
            peak_memory_bytes = working_memory_bytes.saturating_mul(multiplier);
            if !ingestion && working_memory_bytes > 0 {
                held_bytes = Some(working_memory_bytes);
            }
            (0, StorageMedium::Memory)
        }
        // A tumbling-window chain materialized across evaluations holds the
        // tumbling windows its retention covers, all but the newest between
        // evaluations and all while one runs. `state` is one window's.
        (
            false,
            Lifetime::AcrossEvaluations(medium),
            Payload::ASAP(ASAPOp::SummaryTumblingWindowChain { .. }),
        ) => {
            let between = state
                .checked_mul(retained_output_count - 1)
                .ok_or(AnalyticalCostError::Overflow)?;
            match medium {
                StorageMedium::Memory => {
                    average_memory_bytes = between as f64;
                    peak_memory_bytes = state
                        .checked_mul(retained_output_count)
                        .ok_or(AnalyticalCostError::Overflow)?;
                }
                StorageMedium::Disk => disk_bytes = between,
            }
            detail += &format!("; materializes {between} bytes across evaluations");
            (0, medium)
        }
        (_, Lifetime::OneEvaluation(medium), _) => {
            match medium {
                StorageMedium::Memory => {
                    held_bytes = Some(state);
                    peak_memory_bytes = state;
                }
                StorageMedium::Disk => disk_bytes = state,
            }
            (0, medium)
        }
        // Raw rows materialized at ingestion time: the rows it retains.
        (_, Lifetime::AcrossEvaluations(medium), _) if raw_rows_materialized_at_ingestion => {
            (edge(raw_retained_rows.unwrap_or(out.rows))?.bytes, medium)
        }
        // Any other view materialized at ingestion time, a chain included:
        // the outputs its `retention_ms` covers and the one being built. A
        // `TimeShift`'s retention includes its offset: it holds each output
        // that much longer before its readers read it.
        (true, Lifetime::AcrossEvaluations(medium), _) => (
            state
                .checked_mul(
                    retained_output_count
                        .checked_add(1)
                        .ok_or(AnalyticalCostError::Overflow)?,
                )
                .ok_or(AnalyticalCostError::Overflow)?,
            medium,
        ),
        // A whole state materialized across evaluations at query time: it
        // and the one being built.
        (false, Lifetime::AcrossEvaluations(medium), _) => (
            state.checked_mul(2).ok_or(AnalyticalCostError::Overflow)?,
            medium,
        ),
    };
    if ingestion && node.lifetime == Lifetime::AcrossEvaluations(StorageMedium::Memory) {
        maintained = Some(state);
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
    // Query-time memory is held only while the node computes
    // (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
    // section 5, "Memory held while computing"): its bytes for its work per
    // execution, at its executions per second. Query latency does not
    // enter it.
    if let Some(bytes) = held_bytes {
        average_memory_bytes += bytes as f64 * node_ms / 1_000.0 * per_second;
        detail += &format!("; holds {bytes} bytes for {node_ms:.3} ms");
    }
    // One computation's CPU and scan cost times its rate, then the
    // materialized state's.
    let cost = unit_costs.map(|unit_cost| {
        (estimate.cpu_ops() * unit_cost.cost_per_cpu_op
            + estimate.scan_bytes() as f64 * unit_cost.cost_per_scanned_byte)
            * per_second
            + unit_cost.cost_per_memory_byte_second * average_memory_bytes
            + unit_cost.cost_per_disk_byte_second * disk_bytes as f64
    });
    if cost.is_some_and(|cost| !cost.is_finite()) {
        return Err(AnalyticalCostError::Overflow);
    }
    let chain_tumbling_window_width_ms = match &node.payload {
        Payload::ASAP(ASAPOp::SummaryTumblingWindowChain {
            tumbling_window_width_ms,
            ..
        }) => Some(*tumbling_window_width_ms),
        Payload::NonASAP(NonASAPOp::TimeShift { .. }) => chain_tumbling_window_width_ms(producers),
        _ => None,
    };
    Ok(NodeResourceUsageAndCost {
        output: out,
        sources,
        family,
        unchanged_columns,
        tumbling_windows,
        chain_tumbling_window_width_ms,
        work_ms_per_execution: node_ms,
        cpu_ops_per_second: estimate.cpu_ops() * per_second,
        scanned_bytes_per_second: estimate.scan_bytes() as f64 * per_second,
        average_memory_bytes,
        peak_memory_bytes,
        disk_bytes,
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

/// [`PlanCost::source`] for nodes costed as `per_node` says.
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

/// The tumbling-window width of the chain the first of `producers` reads
/// from, if any ([`ProducerOutput::chain_tumbling_window_width_ms`]): a
/// `TimeRange` over it selects windows.
fn chain_tumbling_window_width_ms(producers: &[ProducerOutput<'_>]) -> Option<u64> {
    producers.first()?.chain_tumbling_window_width_ms
}

/// The windows a `TimeRange` of `range` selects from the chain
/// `producers` starts with: those inside the range, at least one.
fn selected_tumbling_windows(
    range: &std::time::Duration,
    producers: &[ProducerOutput<'_>],
) -> Result<u64, AnalyticalCostError> {
    let width_ms = chain_tumbling_window_width_ms(producers)
        .unwrap_or(1)
        .max(1);
    Ok(u64::try_from(range.as_millis() / u128::from(width_ms))
        .map_err(|_| AnalyticalCostError::Overflow)?
        .max(1))
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

#[cfg(test)]
mod tests;
