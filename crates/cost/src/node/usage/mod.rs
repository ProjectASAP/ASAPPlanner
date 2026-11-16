//! One node of a physical plan DAG: its resource usage and cost
//! ([`node_resource_usage_and_cost`], [`NodeCost`]).

// The rules here answer per operator, summary type or cost model, and
// each lists every variant: a wildcard arm would give a new variant a
// default answer instead of failing the build (under `-D warnings`).
// Clippy does not check `matches!` and `if let`, so they stay only for a
// check of one variant's identity, where every other variant's answer is
// "no" by definition: "is it a `Scan`" (a scan's retained raw bytes), and
// `raw_data_samples_non_negative`'s "is its producer a `Scan` or a
// `TimeRange`" (raw samples; any other producer's are not, the safe
// answer). The lifetime check "is it stored across evaluations"
// (`matches!(node.lifetime, Lifetime::AcrossEvaluations(_))`) is kept the
// same way.
#![warn(
    clippy::wildcard_enum_match_arm,
    clippy::match_wildcard_for_single_variants
)]

use std::collections::{BTreeMap, BTreeSet};

use crate::node::analytical::{
    estimate_operator, AnalyticalCostError, PhysicalOperator, ResourceEstimate,
};
use crate::node::operations_per_unit::{
    chain_emit_operations_per_state, estimate_operations_per_group,
    hash_aggregate_operations_per_row, merge_operations_per_state, merge_output_operations,
    pass_through_merge_operations, update_operations_per_row, StateLastWrittenBy,
    CARRIED_LABEL_OPERATIONS_PER_ROW_PER_LABEL, CHAIN_OPERATIONS_PER_ARRIVING_ROW,
    OPERATIONS_PER_GROUP_PER_RAW_BATCH, OUTPUT_ROW_OPERATIONS_PER_GROUP,
    TIME_RANGE_OPERATIONS_PER_ROW,
};
use crate::node::operator_statistics::{
    EdgeStatistics, OperatorStatistics, PartitionStatistics, UnaryEdgeStatistics,
};
use crate::node::state_bytes::{
    keeps_every_value, row_bytes, sketch_state_bytes, state_bytes_per_row,
    state_row_bytes_beside_state, summary_state_bytes, StateInputRows,
};
use crate::{aggregate_lookup, lookup, StateFamily};
use crate::{
    CalibrationFit, CostInputs, NodeCostSource, OperatorKind, COST_SOURCE, MEASURED_COST_SOURCE,
    MIXED_COST_SOURCE,
};
use asap_types::ir::operator::{AggIntent, GroupKeys, Reduction, Source};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::scalar::resolve_column_ref;
use asap_types::ir::schema::{ColumnId, FieldDataType, Schema};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, TimeRangeKind};
use asap_types::plan::physical_asap_dag::{
    PhysicalASAPDAGNode, PhysicalASAPNodeId, PhysicalASAPOperatorPayload as Payload,
};
use asap_types::stages::{Lifetime, StorageMedium};
use asap_types::summary_type::{SketchParams, SketchStatistic, SummaryInputExpr, SummaryUpdate};
use asap_types::workload::bounds::{
    declared_non_negative, declared_series, declared_value_magnitude_range, group_count_bound,
    source_name, source_rows_per_ms, unchanged_source_columns,
};
use asap_types::workload::{DataWorkload, ONE_SECOND_MS};

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
            FieldDataType::Plain(_)
            | FieldDataType::SummarySample(..)
            | FieldDataType::Wavelet(..)
            | FieldDataType::StatModel(..) => StateFamily::None,
        },
        // A merge over a time range of a chain's windows reads states whose
        // producer (the time range) has no family: they are its own θ's, or
        // the exact accumulator its output column holds.
        Payload::ASAP(ASAPOp::SummaryMerge { .. }) => match producer_family {
            None | Some(StateFamily::None) => merged_state_family(node),
            Some(family) => family,
        },
        Payload::ASAP(ASAPOp::SummaryEstimate { .. } | ASAPOp::FinalizeExactAccumulator { .. }) => {
            producer_family.unwrap_or(StateFamily::None)
        }
        Payload::NonASAP(
            NonASAPOp::Scan { .. }
            | NonASAPOp::Values { .. }
            | NonASAPOp::Filter { .. }
            | NonASAPOp::Project { .. }
            | NonASAPOp::Aggregate { .. }
            | NonASAPOp::Join { .. }
            | NonASAPOp::SetOp { .. }
            | NonASAPOp::Concat { .. }
            | NonASAPOp::Dedup { .. }
            | NonASAPOp::Sort { .. }
            | NonASAPOp::Limit { .. }
            | NonASAPOp::BinaryOp { .. }
            | NonASAPOp::SQLWindowFunc { .. }
            | NonASAPOp::TimeRange { .. }
            | NonASAPOp::TimeShift { .. }
            | NonASAPOp::PromqlVectorFromScalar(..)
            | NonASAPOp::PromqlRelabel { .. }
            | NonASAPOp::PromqlInfoEnrich { .. }
            | NonASAPOp::PromqlSeriesSample { .. }
            | NonASAPOp::PromqlSubquery { .. },
        )
        | Payload::ASAP(
            ASAPOp::MaintainPopulation { .. }
            | ASAPOp::EvaluatePopulation { .. }
            | ASAPOp::SummarySubtract { .. }
            | ASAPOp::SummaryDelete { .. }
            | ASAPOp::SummaryJoin { .. }
            | ASAPOp::Extension { .. },
        ) => StateFamily::None,
    })
}

/// The family of the states merge `node` outputs: its θ for a sketch, else
/// the exact accumulator of its output's state column, else none.
fn merged_state_family(
    node: &asap_types::plan::physical_asap_dag::PhysicalASAPDAGNode,
) -> StateFamily<'_> {
    if let Some(parameters) = &node.parameters {
        return StateFamily::Sketch(parameters);
    }
    node.output_schema
        .fields
        .iter()
        .find_map(|field| match &field.dtype {
            FieldDataType::ExactAggregate(kind, _) => Some(StateFamily::Exact(kind)),
            FieldDataType::Plain(_)
            | FieldDataType::Sketch(..)
            | FieldDataType::SummarySample(..)
            | FieldDataType::Wavelet(..)
            | FieldDataType::StatModel(..) => None,
        })
        .unwrap_or(StateFamily::None)
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
    /// from a chain (through the operators that keep each row's window),
    /// 1 for any other node.
    pub tumbling_windows: u64,
    /// The tumbling-window width of the chain whose states its rows are
    /// read from: a chain's own, kept through the operators over it that
    /// keep each row's window (`Operator::keeps_input_window`: `TimeShift`,
    /// `Filter`, `Project`), so a `TimeRange` over them selects windows;
    /// `None` for any other node.
    pub chain_tumbling_window_width_ms: Option<u64>,
    /// What each summary state it outputs folds: the rows a `SummaryAgg`
    /// or chain folds per state, a `SummaryMerge`'s input rows times the
    /// states it merges per group, passed on by a `TimeRange` and the
    /// operators that keep each row's window; `None` for any other node
    /// or when unknown. Example: a 1h `TimeRange` over a chain of 10 s
    /// windows of a 1 Hz series passes on 10 rows per state, and a merge
    /// over it outputs states of 3,600 rows.
    pub state_input_rows: Option<StateInputRows>,
    /// The time, in ms, that the raw rows it outputs span: a scan's
    /// lookback (one second of arriving rows at ingestion time, the
    /// retained rows of raw data materialized at ingestion), narrowed by a
    /// `TimeRange`, passed on by the operators that keep each row's window
    /// and by a `Sort`, `Limit` or `Dedup` (a `Filter` or a `Limit` keeps
    /// the span of the rows it drops), the longest of a `Concat`'s raw
    /// inputs'; `None` for any other node, whose rows are not raw rows. A grouping counts the raw
    /// batches its input arrives in from it. Example: `latency > 100` over
    /// a 5 min scan spans 300,000 ms, 30 batches of 10 s, however few rows
    /// it keeps.
    pub raw_rows_span_ms: Option<u64>,
}

impl ProducerOutput<'_> {
    /// The source column `weight` reads, when this producer passes that
    /// column on unchanged from the sources: the column whose values a
    /// summary built over this producer folds. E.g. `value` for a DDSketch
    /// over a scan of `latency` or over a filter on that scan; `None` for a
    /// computed weight such as a rate.
    pub(crate) fn unchanged_column(&self, weight: &SummaryInputExpr) -> Option<&str> {
        let SummaryInputExpr::Column(column) = weight else {
            return None;
        };
        let index = resolve_column_ref(column, self.output_schema).ok()?;
        let name = self.output_schema.fields.get(index)?.name.as_str();
        self.unchanged_columns.contains(name).then_some(name)
    }
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
    /// from a chain (through the operators that keep each row's window),
    /// 1 for any other node.
    pub tumbling_windows: u64,
    /// The tumbling-window width of the chain whose states its rows are
    /// read from: a chain's own, kept through the operators over it that
    /// keep each row's window (`Operator::keeps_input_window`: `TimeShift`,
    /// `Filter`, `Project`), so a `TimeRange` over them selects windows;
    /// `None` for any other node.
    pub chain_tumbling_window_width_ms: Option<u64>,
    /// What each summary state it outputs folds
    /// ([`ProducerOutput::state_input_rows`]).
    pub state_input_rows: Option<StateInputRows>,
    /// The time its raw output rows span
    /// ([`ProducerOutput::raw_rows_span_ms`]).
    pub raw_rows_span_ms: Option<u64>,
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
            state_input_rows: self.state_input_rows,
            raw_rows_span_ms: self.raw_rows_span_ms,
        }
    }
}

/// Whether `consumer` reads only one batch of `producer`'s output: it runs
/// at ingestion time and `producer` keeps raw rows stored at ingestion
/// (`PhysicalASAPDAGNode::keeps_raw_rows_at_ingestion`, over
/// `producer_input`, its first producer's operator), whose output is the
/// rows it retains, what a query-time reader reads. Such a reader reads
/// the arriving batch, as from a `Scan` that is not materialized; Stage 3
/// and [`plan_cost`](crate::plan_cost) both cost it so.
pub fn reads_one_batch_of<C>(
    consumer: &PhysicalASAPDAGNode,
    producer: &PhysicalASAPDAGNode,
    producer_input: Option<&Operator<C>>,
) -> bool {
    consumer.output_state.timing == ExecutionTiming::IngestionTime
        && producer.keeps_raw_rows_at_ingestion(producer_input)
}

/// The time between two outputs of a node running `payload` that keeps
/// rows, which its retention is divided by: a chain's tumbling window,
/// else `window_ms` (the window it maintains), else one second of
/// arriving rows; at least 1 ms. Stage 3's $\pi_v$ is the cost of one
/// more of it.
pub fn output_interval_ms<C>(payload: &Operator<C>, window_ms: Option<u64>) -> u64 {
    payload
        .chain_tumbling_window_width_ms()
        .unwrap_or_else(|| window_ms.unwrap_or(ONE_SECOND_MS))
        .max(1)
}

/// Measure and cost one node of a physical DAG (one step of
/// [`plan_cost`]; Stage 3 calls it per candidate): `producers` are its
/// producers in its payload's input order (`Operator::children`), `rate`
/// its `(multiplier, per_second)` ([`computations`],
/// [`executions_per_second`]), and `window_ms` the time
/// window one execution covers: an ingestion-time node's refresh window
/// width (`RefreshWindow::computed_from`, the rule the installed plan
/// refreshes it on), or a query-time `Scan`'s read, the
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
    let unit_costs = settings.cost_model.per_node_unit_costs();
    let sketch_size =
        |params: &SketchParams| sketch_state_bytes(params, StateInputRows::UNKNOWN) as f64;
    let mut fit = CalibrationFit::default();
    // Query-time memory held while the node computes.
    let mut held_bytes = None;
    let mut maintained = None;
    let mut raw_scan = None;
    // A query-time scan's raw rows retained.
    let mut raw_retained_rows = None;
    // What each summary state it outputs folds, when it builds or merges
    // states (else passed on below).
    let mut built_state_input_rows: Option<StateInputRows> = None;
    // The time its raw output rows span, when it reads or narrows raw rows
    // (else passed on below).
    let mut read_raw_rows_span_ms: Option<u64> = None;
    let ingestion = node.output_state.timing == ExecutionTiming::IngestionTime;
    // The outputs a node retains: its `retention_ms` over the time between
    // two outputs, at least one. A chain's outputs are its tumbling
    // windows; any other view materialized at ingestion time has one output
    // per refresh (per window it maintains, `window_ms`; per second of
    // arriving rows without one).
    let output_interval_ms = output_interval_ms(&node.payload, window_ms);
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
    // a chain (through the operators that keep each row's window, which
    // pass them on: `Operator::keeps_input_window`), 1 otherwise.
    let tumbling_windows = match &node.payload {
        Payload::NonASAP(NonASAPOp::TimeRange { range, .. })
            if chain_tumbling_window_width_ms(producers).is_some() =>
        {
            selected_tumbling_windows(range, producers)?
        }
        payload if payload.keeps_input_window() => producers
            .first()
            .map_or(1, |producer| producer.tumbling_windows),
        // Any other operator spans one window: `keeps_input_window`
        // answers per operator, so listing them here would decide nothing.
        Payload::NonASAP(_) | Payload::ASAP(_) => 1,
    };
    let sources: BTreeSet<&str> =
        if let Payload::NonASAP(NonASAPOp::Scan { source, .. }) = &node.payload {
            [source_name(source)].into()
        } else {
            producers
                .iter()
                .flat_map(|p| p.sources.iter().copied())
                .collect()
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
    let raw_rows_materialized_at_ingestion =
        node.keeps_raw_rows_at_ingestion(producers.first().map(|producer| producer.payload));
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
        Payload::NonASAP(
            NonASAPOp::Scan { .. }
            | NonASAPOp::Values { .. }
            | NonASAPOp::Filter { .. }
            | NonASAPOp::Project { .. }
            | NonASAPOp::Join { .. }
            | NonASAPOp::SetOp { .. }
            | NonASAPOp::Concat { .. }
            | NonASAPOp::Dedup { .. }
            | NonASAPOp::Sort { .. }
            | NonASAPOp::Limit { .. }
            | NonASAPOp::BinaryOp { .. }
            | NonASAPOp::SQLWindowFunc { .. }
            | NonASAPOp::TimeRange { .. }
            | NonASAPOp::TimeShift { .. }
            | NonASAPOp::PromqlVectorFromScalar(..)
            | NonASAPOp::PromqlRelabel { .. }
            | NonASAPOp::PromqlInfoEnrich { .. }
            | NonASAPOp::PromqlSeriesSample { .. }
            | NonASAPOp::PromqlSubquery { .. },
        )
        | Payload::ASAP(
            ASAPOp::SummaryAgg { .. }
            | ASAPOp::SummaryEstimate { .. }
            | ASAPOp::FinalizeExactAccumulator { .. }
            | ASAPOp::MaintainPopulation { .. }
            | ASAPOp::EvaluatePopulation { .. }
            | ASAPOp::SummaryMerge { .. }
            | ASAPOp::SummaryTumblingWindowChain { .. }
            | ASAPOp::SummarySubtract { .. }
            | ASAPOp::SummaryDelete { .. }
            | ASAPOp::SummaryJoin { .. }
            | ASAPOp::Extension { .. },
        ) => lookup(
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
    // What the sources declare about the values a summary built here folds:
    // whether every one is non-negative, and their magnitude range, both
    // from the one source column it folds
    // (`ProducerOutput::unchanged_column`).
    let folded_values = |update: &SummaryUpdate| {
        producers
            .first()
            .and_then(|producer| producer.unchanged_column(&update.weight))
            .map_or((false, None), |column| {
                let non_negative = !sources.is_empty()
                    && sources
                        .iter()
                        .all(|source| declared_non_negative(data, source, column));
                let range = declared_value_magnitude_range(data, sources.iter().copied(), column);
                (non_negative, range)
            })
    };
    // The distinct values a source column passed on unchanged takes per
    // evaluation, as every source declares them
    // (`distinct_values_per_label`, summed over the sources); `None` for a
    // computed column or when one does not.
    let declared_distinct_values = |column: Option<&str>| -> Option<u64> {
        let column = column?;
        if sources.is_empty() {
            return None;
        }
        sources.iter().try_fold(0_u64, |total, source| {
            Some(
                total.saturating_add(
                    *data
                        .sources
                        .get(*source)?
                        .distinct_values_per_label
                        .get(column)?,
                ),
            )
        })
    };
    // The distinct items one output row's state holds at most per
    // evaluation (`max_distinct_per_evaluation`, summed over the sources).
    let distinct_items_per_group = || -> Option<u64> {
        if sources.is_empty() {
            return None;
        }
        sources.iter().try_fold(0_u64, |total, source| {
            Some(total.saturating_add(data.sources.get(*source)?.max_distinct_per_evaluation?))
        })
    };
    // The values the column a summary built here folds takes in one state:
    // the fewer of its declared distinct values and the sources' bound per
    // output row; `None` when neither is declared.
    let folded_distinct_values = |update: &SummaryUpdate| -> Option<u64> {
        let column = producers
            .first()
            .and_then(|producer| producer.unchanged_column(&update.weight));
        [declared_distinct_values(column), distinct_items_per_group()]
            .into_iter()
            .flatten()
            .min()
    };
    // The bytes of each summary state its first input holds.
    let input_state_bytes = || {
        producers.first().map_or(0, |producer| {
            state_bytes_per_row(producer.output, producer.output_schema)
        })
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
    // The time its input's raw rows span, when its input is raw rows.
    let input_raw_rows_span_ms = producers
        .first()
        .and_then(|producer| producer.raw_rows_span_ms);
    // The times a grouping into `group_count` groups visits a group's
    // state: once per group per batch of raw rows its input arrives in
    // (`group_raw_batch_visits`).
    let visits = |group_count: u64| {
        group_raw_batch_visits(
            input.rows,
            group_count,
            input_raw_rows_span_ms,
            speed.raw_batch_width_ms,
            ingestion,
        )
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
                // The time those rows span: one second at ingestion time,
                // the retention of raw data materialized there, at query
                // time the rows read at the sources' rate (its lookback,
                // less when fewer rows are held).
                read_raw_rows_span_ms = match (ingestion, raw_rows_materialized_at_ingestion) {
                    (_, true) => node.retention_ms,
                    (true, false) => Some(ONE_SECOND_MS),
                    (false, false) => rows_per_ms
                        .filter(|rows_per_ms| *rows_per_ms > 0.0)
                        .map(|rows_per_ms| (out.rows as f64 / rows_per_ms).round() as u64)
                        .or(lookback),
                };
                (out, estimate, format!("scan {} samples", out.rows))
            }
            NonASAPOp::Aggregate {
                reduction,
                measures,
                ..
            } => {
                let group_count = groups(reduction)?;
                let keys = grouping_key_count(reduction);
                let out = edge(group_count)?;
                let held = input.rows.div_ceil(group_count.max(1));
                // The values its distinct counts and frequency statistics
                // read: the most any one's columns take together, as
                // declared for source columns passed on unchanged, else
                // every row's differs; at most the sources' bound on the
                // items any one distinct count over their rows counts
                // (the ungrouped count over every row meets it too, so
                // it bounds the values of all groups together), and the
                // rows read.
                let declared_together = |cols: &[usize]| -> Option<u64> {
                    let unchanged = &producers.first()?.unchanged_columns;
                    cols.iter().try_fold(1_u64, |product, &col| {
                        let name = input_schema?.fields.get(col)?.name.as_str();
                        let values =
                            declared_distinct_values(unchanged.contains(name).then_some(name))?;
                        Some(product.saturating_mul(values))
                    })
                };
                let distinct_values = measures
                    .iter()
                    .filter_map(|measure| {
                        if let AggIntent::Cardinality { cols } = measure {
                            Some(cols.clone())
                        } else if let AggIntent::FrequencyL2 { col }
                        | AggIntent::FrequencyEntropy { col } = measure
                        {
                            Some(col.iter().copied().collect())
                        } else {
                            None
                        }
                    })
                    .map(|cols| {
                        (!cols.is_empty())
                            .then(|| declared_together(&cols))
                            .flatten()
                            .unwrap_or(input.rows)
                    })
                    .max()
                    .into_iter()
                    .chain(distinct_items_per_group())
                    .chain([input.rows])
                    .min()
                    .unwrap_or(input.rows);
                let operations_per_row =
                    hash_aggregate_operations_per_row(
                        keys,
                        measures,
                        input.rows,
                        group_count,
                        distinct_values,
                    ) + carried_label_operations_per_row(&node.output_schema, reduction);
                let group_visits = visits(group_count);
                let estimate = estimate_operator(
                    PhysicalOperator::HashAggregate {
                        grouping_key_count: keys,
                        operations_per_row: operations_per_row.ceil() as u64,
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
                )
                .map(|estimate| {
                    ResourceEstimate::new(
                        estimate.cpu_ops() + group_visits * OPERATIONS_PER_GROUP_PER_RAW_BATCH,
                        estimate.peak_memory_bytes(),
                        estimate.scan_bytes(),
                    )
                });
                (
                    out,
                    estimate,
                    format!(
                        "hash aggregate {} rows into {group_count} groups: {operations_per_row:.0} \
                         ops per row, {group_visits:.0} group visits",
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
                        read_raw_rows_span_ms = node.retention_ms;
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
                // The rows it keeps span its range, at most its input's.
                let range_ms = u64::try_from(range.as_millis()).unwrap_or(u64::MAX);
                read_raw_rows_span_ms = Some(
                    input_raw_rows_span_ms.map_or(range_ms, |input_ms| input_ms.min(range_ms)),
                );
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
            NonASAPOp::Values { .. }
            | NonASAPOp::Filter { .. }
            | NonASAPOp::Project { .. }
            | NonASAPOp::Join { .. }
            | NonASAPOp::SetOp { .. }
            | NonASAPOp::Concat { .. }
            | NonASAPOp::Dedup { .. }
            | NonASAPOp::BinaryOp { .. }
            | NonASAPOp::SQLWindowFunc { .. }
            | NonASAPOp::PromqlVectorFromScalar(..)
            | NonASAPOp::PromqlRelabel { .. }
            | NonASAPOp::PromqlInfoEnrich { .. }
            | NonASAPOp::PromqlSeriesSample { .. }
            | NonASAPOp::PromqlSubquery { .. } => {
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
            input: update,
            reduction,
            ..
        }) => {
            let group_count = groups(reduction)?;
            // At query time, each state sees its group's input rows; at
            // ingestion time, the rows of a window one second of
            // arriving rows does not bound.
            let (non_negative, value_magnitude_range) = folded_values(update);
            let input_rows = StateInputRows {
                rows_per_state: (!ingestion).then(|| input.rows.div_ceil(group_count.max(1))),
                non_negative,
                value_magnitude_range,
            };
            built_state_input_rows = Some(input_rows);
            let state_bytes = measured_state_bytes.unwrap_or(summary_state_bytes(
                family,
                node.parameters.as_ref(),
                input_rows,
            )?);
            let update_ops = update_operations_per_row(
                family,
                node.parameters.as_ref(),
                input_rows.rows_per_state,
                folded_distinct_values(update),
            ) + carried_label_operations_per_row(&node.output_schema, reduction);
            let out = EdgeStatistics {
                rows: group_count,
                bytes: group_count
                    .checked_mul(state_bytes + state_row_bytes_beside_state(&node.output_schema))
                    .ok_or(AnalyticalCostError::Overflow)?,
            };
            let group_visits = visits(group_count);
            (
                out,
                Ok(ResourceEstimate::new(
                    input.rows as f64 * update_ops
                        + group_visits * OPERATIONS_PER_GROUP_PER_RAW_BATCH,
                    out.bytes,
                    0,
                )),
                format!(
                    "build {} into {group_count} states: {} rows x {update_ops} ops, \
                     {group_visits:.0} group visits",
                    summary_type_name(family),
                    input.rows
                ),
            )
        }
        Payload::ASAP(ASAPOp::SummaryTumblingWindowChain {
            summary_type: family,
            input: update,
            reduction,
            tumbling_window_width_ms,
            ..
        }) => {
            // One window's states: each input row updates the window its
            // time falls in. At ingestion time the rows of one window
            // arrive at the rate of one second's `input.rows`; at query
            // time one window holds its width's share of the time its
            // input's raw rows span (without a span, its share of the
            // retained windows).
            let group_count = groups(reduction)?;
            let rows_per_tumbling_window = match ingestion {
                true => u64::try_from(
                    (u128::from(input.rows) * u128::from(*tumbling_window_width_ms))
                        .div_ceil(u128::from(ONE_SECOND_MS)),
                )
                .map_err(|_| AnalyticalCostError::Overflow)?,
                false => input_raw_rows_span_ms
                    .filter(|span_ms| *span_ms > 0)
                    .map_or(input.rows.div_ceil(retained_output_count), |span_ms| {
                        ((input.rows as f64 * *tumbling_window_width_ms as f64 / span_ms as f64)
                            .round() as u64)
                            .clamp(1, input.rows.max(1))
                    }),
            };
            let (non_negative, value_magnitude_range) = folded_values(update);
            let input_rows = StateInputRows {
                rows_per_state: Some(rows_per_tumbling_window.div_ceil(group_count.max(1))),
                non_negative,
                value_magnitude_range,
            };
            built_state_input_rows = Some(input_rows);
            let state_bytes = measured_state_bytes.unwrap_or(summary_state_bytes(
                family,
                node.parameters.as_ref(),
                input_rows,
            )?);
            let update_ops = update_operations_per_row(
                family,
                node.parameters.as_ref(),
                input_rows.rows_per_state,
                folded_distinct_values(update),
            ) + carried_label_operations_per_row(&node.output_schema, reduction);
            // The states it completes in one execution: at ingestion time
            // one second's share of each group's window (a 10 s window: a
            // tenth of a state per group); at query time every retained
            // window's.
            let emitted_states = match ingestion {
                true => {
                    group_count as f64 * ONE_SECOND_MS as f64 / *tumbling_window_width_ms as f64
                }
                false => group_count as f64 * retained_output_count as f64,
            };
            // At query time each row of the scan's lookback is kept in its
            // window's range, the work a `TimeRange` over the same rows does
            // (11 per row); at ingestion time each arriving row is routed to
            // its window's state (14 per row).
            let range_ops = input.rows as f64
                * match ingestion {
                    true => CHAIN_OPERATIONS_PER_ARRIVING_ROW,
                    false => TIME_RANGE_OPERATIONS_PER_ROW,
                };
            // Each group's state found once per raw batch.
            let group_visits = visits(group_count);
            let out = EdgeStatistics {
                rows: group_count,
                bytes: group_count
                    .checked_mul(state_bytes + state_row_bytes_beside_state(&node.output_schema))
                    .ok_or(AnalyticalCostError::Overflow)?,
            };
            (
                out,
                Ok(ResourceEstimate::new(
                    input.rows as f64 * update_ops
                        + range_ops
                        + group_visits * OPERATIONS_PER_GROUP_PER_RAW_BATCH
                        + emitted_states * chain_emit_operations_per_state(family),
                    out.bytes,
                    0,
                )),
                format!(
                    "build {} in tumbling windows of {tumbling_window_width_ms} ms for \
                     {group_count} groups: {} rows x {update_ops} ops, {group_visits:.0} \
                     group visits, {emitted_states:.1} states emitted",
                    summary_type_name(family),
                    input.rows
                ),
            )
        }
        Payload::ASAP(ASAPOp::SummaryMerge { group_by, .. }) => {
            // Each input state read is merged into its group's (by the
            // states' family, `merge_ops` below); one state per group
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
            // The merged state: each group's input states folded into one,
            // sized for the rows they fold together when every input says
            // what its states fold (a KLL grows towards its `k`), else as
            // the largest input state. That fallback under-charges a KLL
            // merge, whose state grows past any one input's; it applies
            // when an input passes on no `state_input_rows`, e.g. states
            // read through an operator that does not keep each row's
            // window.
            let input_state_bytes = |producer: &ProducerOutput<'_>| {
                state_bytes_per_row(producer.output, producer.output_schema)
            };
            // Each input's states per group, every one folding its rows.
            let merged_input_rows = StateInputRows::merged(producers.iter().map(|producer| {
                producer
                    .state_input_rows
                    .map(|rows| (rows, producer.output.rows.div_ceil(group_count.max(1))))
            }));
            built_state_input_rows = merged_input_rows;
            let merged_state_bytes = match (family, merged_input_rows) {
                (StateFamily::Sketch(params), Some(rows)) => sketch_state_bytes(params, rows),
                _ => producers.iter().map(input_state_bytes).max().unwrap_or(0),
            };
            // Each input state's merge, by the states' family and bytes,
            // and each output group's state and row. A merge that keeps
            // its input's groups (no `group_by`, or the input's own keys)
            // and reads one state per group passes each state through
            // unmerged. A rollup merges even when its estimate ties its
            // input's: states by (region, service) rolled up by `service`
            // merge several per service.
            let keeps_input_groups = group_by.as_ref().is_none_or(|keys| {
                producers
                    .iter()
                    .all(|producer| keys_are_input_keys(keys, producer.output_schema))
            });
            let merge_ops: f64 = match keeps_input_groups && states <= group_count {
                true => pass_through_merge_operations(group_count),
                false => {
                    producers
                        .iter()
                        .map(|producer| {
                            producer.output.rows as f64
                                * merge_operations_per_state(
                                    family,
                                    input_state_bytes(producer),
                                    merged_state_bytes,
                                    producer
                                        .state_input_rows
                                        .and_then(|rows| rows.rows_per_state),
                                    states.div_ceil(group_count.max(1)),
                                )
                        })
                        .sum::<f64>()
                        + merge_output_operations(family, group_count)
                }
            };
            let out = EdgeStatistics {
                rows: group_count,
                bytes: (merged_state_bytes + state_row_bytes_beside_state(&node.output_schema))
                    .checked_mul(group_count)
                    .ok_or(AnalyticalCostError::Overflow)?,
            };
            (
                out,
                Ok(ResourceEstimate::new(merge_ops, out.bytes, 0)),
                format!("merge {states} states into {group_count}"),
            )
        }
        Payload::ASAP(ASAPOp::SummaryEstimate { query, .. }) => {
            // A merge leaves a KLL's levels sorted; a state built by
            // updates (a `SummaryAgg`, a chain's window) is sorted when read.
            let written_by = match producers.first() {
                Some(producer)
                    if matches!(producer.payload, Payload::ASAP(ASAPOp::SummaryMerge { .. })) =>
                {
                    StateLastWrittenBy::Merge
                }
                producer => StateLastWrittenBy::Updates {
                    rows_per_state: producer
                        .and_then(|producer| producer.state_input_rows)
                        .and_then(|rows| rows.rows_per_state),
                },
            };
            let per_group = match query {
                SketchStatistic::TopK { k } => *k as u64,
                SketchStatistic::FrequencyL2
                | SketchStatistic::FrequencyEntropy
                | SketchStatistic::Quantile { .. }
                | SketchStatistic::PointCount { .. }
                | SketchStatistic::Cardinality => 1,
            };
            let out = edge(
                input
                    .rows
                    .checked_mul(per_group)
                    .ok_or(AnalyticalCostError::Overflow)?,
            )?;
            (
                out,
                Ok(ResourceEstimate::new(
                    input.rows as f64
                        * estimate_operations_per_group(family, input_state_bytes(), written_by),
                    0,
                    0,
                )),
                format!("estimate {} groups x {per_group}", input.rows),
            )
        }
        Payload::ASAP(ASAPOp::FinalizeExactAccumulator { .. }) => (
            edge(input.rows)?,
            // An exact accumulator's read is its output row alone.
            Ok(ResourceEstimate::new(
                input.rows as f64 * OUTPUT_ROW_OPERATIONS_PER_GROUP,
                0,
                0,
            )),
            format!("finalize {} accumulators", input.rows),
        ),
        Payload::ASAP(
            ASAPOp::MaintainPopulation { .. }
            | ASAPOp::EvaluatePopulation { .. }
            | ASAPOp::SummarySubtract { .. }
            | ASAPOp::SummaryDelete { .. }
            | ASAPOp::SummaryJoin { .. }
            | ASAPOp::Extension { .. },
        ) => (
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
        Payload::NonASAP(
            NonASAPOp::Values { .. }
            | NonASAPOp::Filter { .. }
            | NonASAPOp::Project { .. }
            | NonASAPOp::Aggregate { .. }
            | NonASAPOp::Join { .. }
            | NonASAPOp::SetOp { .. }
            | NonASAPOp::Concat { .. }
            | NonASAPOp::Dedup { .. }
            | NonASAPOp::Sort { .. }
            | NonASAPOp::Limit { .. }
            | NonASAPOp::BinaryOp { .. }
            | NonASAPOp::SQLWindowFunc { .. }
            | NonASAPOp::TimeRange { .. }
            | NonASAPOp::TimeShift { .. }
            | NonASAPOp::PromqlVectorFromScalar(..)
            | NonASAPOp::PromqlRelabel { .. }
            | NonASAPOp::PromqlInfoEnrich { .. }
            | NonASAPOp::PromqlSeriesSample { .. }
            | NonASAPOp::PromqlSubquery { .. },
        )
        | Payload::ASAP(
            ASAPOp::SummaryAgg { .. }
            | ASAPOp::SummaryEstimate { .. }
            | ASAPOp::FinalizeExactAccumulator { .. }
            | ASAPOp::MaintainPopulation { .. }
            | ASAPOp::EvaluatePopulation { .. }
            | ASAPOp::SummaryTumblingWindowChain { .. }
            | ASAPOp::SummarySubtract { .. }
            | ASAPOp::SummaryDelete { .. }
            | ASAPOp::SummaryJoin { .. }
            | ASAPOp::Extension { .. },
        ) => input.rows,
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
    let chain_tumbling_window_width_ms =
        node.payload.chain_tumbling_window_width_ms().or_else(|| {
            node.payload
                .keeps_input_window()
                .then(|| chain_tumbling_window_width_ms(producers))
                .flatten()
        });
    // A node that builds or merges states says what they fold; a time
    // range and an operator keeping each row's window pass on their
    // input's.
    let state_input_rows = built_state_input_rows.or_else(|| {
        let passes_states_on =
            matches!(node.payload, Payload::NonASAP(NonASAPOp::TimeRange { .. }))
                || node.payload.keeps_input_window();
        passes_states_on
            .then(|| {
                producers
                    .first()
                    .and_then(|producer| producer.state_input_rows)
            })
            .flatten()
    });
    // A scan and a time range over raw rows say what their rows span; an
    // operator keeping each row's window passes on its input's, and so do
    // a sort, a limit and a dedup, which output raw rows as they are (a
    // limit and a dedup drop some, as a filter does). A concat of raw rows
    // spans the longest of its inputs'.
    let passes_raw_rows_on = node.payload.keeps_input_window()
        || matches!(
            node.payload,
            Payload::NonASAP(
                NonASAPOp::Sort { .. } | NonASAPOp::Limit { .. } | NonASAPOp::Dedup { .. }
            )
        );
    let raw_rows_span_ms = read_raw_rows_span_ms.or_else(|| {
        if matches!(node.payload, Payload::NonASAP(NonASAPOp::Concat { .. })) {
            return producers
                .iter()
                .map(|producer| producer.raw_rows_span_ms)
                .collect::<Option<Vec<_>>>()?
                .into_iter()
                .max();
        }
        passes_raw_rows_on
            .then_some(input_raw_rows_span_ms)
            .flatten()
    });
    Ok(NodeResourceUsageAndCost {
        output: out,
        sources,
        family,
        unchanged_columns,
        tumbling_windows,
        chain_tumbling_window_width_ms,
        state_input_rows,
        raw_rows_span_ms,
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

/// The times a grouping into `group_count` groups visits a group's state
/// while reading `input_rows` rows: once per group per batch of raw rows
/// (`raw_batch_width_ms` each) its input arrives in, at most once per row.
/// The batches are the time its input's raw rows span
/// (`input_raw_rows_span_ms`) over a batch's: at query time at least one;
/// at ingestion time a share of one per execution, one second over a
/// batch's width. Input that is not raw rows: one batch at query time, a
/// second's share of one at ingestion time. Example: a 5 min window of
/// 1,000 series at 1 Hz (300,000 rows) in 10 s batches spans 30 batches,
/// 30,000 visits; at 10 Hz the same 30,000 visits for 3,000,000 rows, and
/// under a filter keeping a tenth of each series' rows the same 30,000
/// for 30,000 rows.
fn group_raw_batch_visits(
    input_rows: u64,
    group_count: u64,
    input_raw_rows_span_ms: Option<u64>,
    raw_batch_width_ms: u64,
    ingestion: bool,
) -> f64 {
    let batch_width_ms = raw_batch_width_ms.max(1) as f64;
    let batches = match input_raw_rows_span_ms {
        Some(span_ms) => span_ms as f64 / batch_width_ms,
        None => match ingestion {
            true => ONE_SECOND_MS as f64 / batch_width_ms,
            false => 1.0,
        },
    };
    let batches = match ingestion {
        true => batches,
        false => batches.max(1.0),
    };
    (group_count.max(1) as f64 * batches).min(input_rows as f64)
}

/// The grouping keys `reduction` hashes per row: its keys, or the entity
/// (one) when it keeps one row per entity.
fn grouping_key_count(reduction: &Reduction) -> u64 {
    match reduction {
        Reduction::Reduce(keys) => keys.keys().len() as u64,
        Reduction::PerEntity => 1,
    }
}

/// The operations per input row a grouping by `reduction` pays for the
/// label columns it carries per group beside its keys, which each input
/// row copies: the string columns of its `output_schema` that are not
/// keys, at `CARRIED_LABEL_OPERATIONS_PER_ROW_PER_LABEL` each. Example:
/// the per-series sum under `sum by (job) (sum_over_time(latency[5m]))`
/// outputs the series identity (its key) and `job`, so it carries one
/// label: 37 operations per row.
fn carried_label_operations_per_row(output_schema: &Schema, reduction: &Reduction) -> f64 {
    let label_columns = output_schema
        .fields
        .iter()
        .filter(|field| field.dtype == FieldDataType::Plain(asap_types::ir::schema::DataType::Utf8))
        .count() as u64;
    let carried = label_columns.saturating_sub(grouping_key_count(reduction));
    carried as f64 * CARRIED_LABEL_OPERATIONS_PER_ROW_PER_LABEL
}

/// The input states a `SummaryMerge` over `producers` reads: every row of
/// each.
/// Whether a rollup's `keys` are its input's own keys: every plain
/// column of `input` but its time index, in any order. Example: over
/// states keyed by (window_end, region, service), `[2, 1]` is, `[2]` is
/// not.
fn keys_are_input_keys(keys: &[ColumnId], input: &Schema) -> bool {
    let input_keys: BTreeSet<ColumnId> = input
        .fields
        .iter()
        .enumerate()
        .filter(|(column, field)| field.is_plain() && input.time_index != Some(*column))
        .map(|(column, _)| column)
        .collect();
    keys.iter().copied().collect::<BTreeSet<_>>() == input_keys
}

fn merged_states(producers: &[ProducerOutput<'_>]) -> Result<u64, AnalyticalCostError> {
    producers.iter().try_fold(0_u64, |total, producer| {
        total
            .checked_add(producer.output.rows)
            .ok_or(AnalyticalCostError::Overflow)
    })
}

/// A summary type's name in reports: a sketch by its algorithm, e.g.
/// `HydraCms`; an exact accumulator by its kind.
// Any other type prints its Debug form, which names a new variant too, so
// listing them would decide nothing.
#[allow(clippy::wildcard_enum_match_arm)]
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
