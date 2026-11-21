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
    estimate_operator_with_terms, AnalyticalCostError, PhysicalOperator, ResourceEstimate,
};
use crate::node::cost_terms::{CostConstant, NodeCostTerms};
use crate::node::operations_per_unit::{
    chain_emit_terms_per_state, estimate_terms_per_group, hash_aggregate_terms_per_row,
    merge_output_terms, merge_terms_per_state, pass_through_merge_terms, state_allocation_terms,
    update_terms_per_row, StateLastWrittenBy,
};
use crate::node::operator_statistics::{
    EdgeStatistics, OperatorStatistics, PartitionStatistics, UnaryEdgeStatistics,
};
use crate::node::state_bytes::{
    assumed_magnitude_range, keeps_every_value, open_sketch_state_bytes, open_summary_state_bytes,
    row_bytes, sketch_state_bytes, state_bytes_per_row, state_row_bytes_beside_state,
    summary_state_bytes, StateInputRows,
};
use crate::{aggregate_lookup, lookup_at_scale, StateFamily};
use crate::{
    CalibrationFit, CostInputs, NodeCostSource, OperatorKind, COST_SOURCE, MEASURED_COST_SOURCE,
    MIXED_COST_SOURCE,
};
use asap_types::ir::operator::tumbling_window_chain::mean_cover_windows_per_level;
use asap_types::ir::operator::{AggIntent, GroupKeys, Reduction, Source};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::scalar::resolve_column_ref;
use asap_types::ir::schema::{ColumnId, FieldDataType, Schema};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, TimeRangeKind};
use asap_types::plan::physical_asap_dag::{
    PhysicalASAPDAGNode, PhysicalASAPNodeId, PhysicalASAPOperatorPayload as Payload,
};
use asap_types::stages::EvaluationGrid;
use asap_types::stages::{Lifetime, StorageMedium};
use asap_types::summary_type::{
    SamplingKind, SamplingParams, SketchParams, SketchStatistic, StatModelKind, StatModelParams,
    SummaryInputExpr, SummaryUpdate, WaveletKind, WaveletParams,
};
use asap_types::workload::bounds::{
    declared_non_negative, declared_series, declared_value_magnitude_range, group_count_bound,
    source_name, source_rows_per_ms, unchanged_source_columns,
};
use asap_types::workload::{AssumedDataFact, DataFactAssumption, DataWorkload, ONE_SECOND_MS};

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
    /// The data facts the data workload does not declare that its cost
    /// assumed, each once, e.g. a DDSketch's value magnitude range.
    pub assumed_data_facts: Vec<AssumedDataFact>,
    /// Its time by phase ([`PhaseTimes`]).
    pub phase_times: PhaseTimes,
    /// Whether its measured price was taken from the nearest measured
    /// series count, its own being outside them
    /// ([`lookup_at_scale`](crate::lookup_at_scale)).
    pub priced_outside_measured_scale: bool,
}

/// One node's time by phase, what a runtime measures per phase. An
/// ingestion-time node's work splits into ingest work, per arriving row on
/// the ingest threads, and refresh work, once per window close on the
/// precompute threads: the states a build or chain emits, and all the
/// work of an ingestion-time node that reads closed windows (summary
/// states or a chain's windows, not raw rows). A query-time node has only
/// its time per step.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PhaseTimes {
    /// Ingest work: core-seconds per second.
    pub ingest_cpu_core_seconds_per_second: f64,
    /// Refresh work: CPU ms per window close; 0 without a refresh window.
    pub refresh_cpu_ms_per_window: f64,
    /// Refresh work's wall time per window close: its CPU plus the I/O
    /// wait per state emitted
    /// ([`RefreshIoWait`](crate::RefreshIoWait)); the wait per refresh run
    /// is the plan's, once per refresh group.
    pub refresh_wall_ms_per_window: f64,
    /// The refresh window's width in ms; `None` without one.
    pub refresh_window_width_ms: Option<u64>,
    /// At query time: ms of one computation (one step).
    pub query_ms_per_step: f64,
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
            FieldDataType::SummarySample(kind, params) => {
                sampling_state_family(kind, params).unwrap_or(StateFamily::None)
            }
            FieldDataType::Wavelet(kind, params) => {
                wavelet_state_family(kind, params).unwrap_or(StateFamily::None)
            }
            FieldDataType::StatModel(kind, params) => {
                stat_model_state_family(kind, params).unwrap_or(StateFamily::None)
            }
            FieldDataType::Plain(_) => StateFamily::None,
        },
        // A merge over a time range of a chain's windows reads states whose
        // producer (the time range) has no family: they are its own θ's, or
        // the exact accumulator its output column holds.
        Payload::ASAP(ASAPOp::SummaryMerge { .. } | ASAPOp::SummaryMergeWindows { .. }) => {
            match producer_family {
                None | Some(StateFamily::None) => merged_state_family(node),
                Some(family) => family,
            }
        }
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
            | NonASAPOp::TimeRangeCover { .. }
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
            FieldDataType::SummarySample(kind, params) => sampling_state_family(kind, params),
            FieldDataType::Wavelet(kind, params) => wavelet_state_family(kind, params),
            FieldDataType::StatModel(kind, params) => stat_model_state_family(kind, params),
            FieldDataType::Plain(_) | FieldDataType::Sketch(..) => None,
        })
        .unwrap_or(StateFamily::None)
}

/// The measured-cost family of a sampling summary's states: none yet, so
/// its nodes are costed without a family's entries.
fn sampling_state_family<'a>(
    _kind: &SamplingKind,
    _params: &SamplingParams,
) -> Option<StateFamily<'a>> {
    // TODO: implement when an executor builds sampling summaries.
    None
}

/// The measured-cost family of a wavelet summary's states: none yet, so its
/// nodes are costed without a family's entries.
fn wavelet_state_family<'a>(
    _kind: &WaveletKind,
    _params: &WaveletParams,
) -> Option<StateFamily<'a>> {
    // TODO: implement when an executor builds wavelet summaries.
    None
}

/// The measured-cost family of a statistical-model summary's states: none
/// yet, so its nodes are costed without a family's entries.
fn stat_model_state_family<'a>(
    _kind: &StatModelKind,
    _params: &StatModelParams,
) -> Option<StateFamily<'a>> {
    // TODO: implement when an executor builds statistical-model summaries.
    None
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
    pub tumbling_windows: f64,
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
    /// The exact rate, in rows per ms, at which its raw output rows
    /// arrive, while its rows are that rate over their span
    /// (`raw_rows_span_ms`): a scan's sources' arrival rate, passed on by a
    /// `TimeRange` and the operators that keep each row's window; `None`
    /// once a node outputs other rows (a `Limit`, a `Dedup`, rows bounded
    /// by `rows_held`). Example: 10,000 series every 15 s arrive at
    /// 0.6667 rows per ms, which one second's `rows` rounds to 667.
    pub raw_rows_per_ms: Option<f64>,
}

impl ProducerOutput<'_> {
    /// The source column `expr` reads, when `expr` is a single column this
    /// producer passes on unchanged from the sources: the column whose
    /// values a summary built over this producer folds, its weight or a
    /// keyed summary's item. E.g. `value` for a DDSketch over a scan of
    /// `latency` or over a filter on that scan, `url` for a top-k heap keyed
    /// by `url`; `None` for a computed weight such as a rate, or a tuple
    /// or entity-identity item.
    pub(crate) fn unchanged_column(&self, expr: &SummaryInputExpr) -> Option<&str> {
        let SummaryInputExpr::Column(column) = expr else {
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
    pub tumbling_windows: f64,
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
    /// The exact rate its raw output rows arrive at
    /// ([`ProducerOutput::raw_rows_per_ms`]).
    pub raw_rows_per_ms: Option<f64>,
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
    /// The series its sources declare, when they all do: the scale its
    /// measured prices are taken at.
    pub series: Option<u64>,
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
            raw_rows_per_ms: self.raw_rows_per_ms,
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
/// most offset + range its consumers read ([`read_back_ms`](crate::read_back_ms) without a span;
/// `None`: it reads what it retains). A materialized node keeps what its
/// `retention_ms` ($h_v$) covers: a tumbling-window chain its windows in
/// it, a query-time `Scan` and raw rows materialized at ingestion time (a
/// `Scan`, or a `TimeShift` by an offset over one) their raw rows in it,
/// any other view materialized at ingestion time one output per refresh in
/// it (one second of rows when it maintains no window). The formulas' CPU is scaled by
/// `settings.frozen_calibration_factor` (1 when unset).
///
/// `evaluation_grid` is the grid of the evaluation times the node runs at
/// (its readers'; `None` when not one grid): a `TimeRangeCover` is charged
/// its mean windows over those times. Example: a cover of 1 h over 5 min
/// and 15 s windows read every 60 s takes 27.2 windows per evaluation,
/// every 15 s 30.05.
///
/// [`executions_per_second`]: crate::plan::execution_demand::executions_per_second
pub fn node_resource_usage_and_cost<'a>(
    node: &'a asap_types::plan::physical_asap_dag::PhysicalASAPDAGNode,
    producers: &[ProducerOutput<'a>],
    rate: (u64, f64),
    window_ms: Option<u64>,
    evaluation_grid: Option<EvaluationGrid>,
    data: &DataWorkload,
    settings: &'a CostInputs,
) -> Result<NodeResourceUsageAndCost<'a>, AnalyticalCostError> {
    node_usage_and_cost_terms(
        node,
        producers,
        rate,
        window_ms,
        evaluation_grid,
        data,
        settings,
    )
    .map(|(usage, _)| usage)
}

/// [`node_resource_usage_and_cost`] and the terms of its formula CPU per
/// execution ([`crate::node_cost_terms`]). The formula CPU is computed from
/// those terms, so the two cannot drift; the terms are taken before a
/// measured entry or the calibration factor replaces or scales it.
pub(crate) fn node_usage_and_cost_terms<'a>(
    node: &'a asap_types::plan::physical_asap_dag::PhysicalASAPDAGNode,
    producers: &[ProducerOutput<'a>],
    rate: (u64, f64),
    window_ms: Option<u64>,
    evaluation_grid: Option<EvaluationGrid>,
    data: &DataWorkload,
    settings: &'a CostInputs,
) -> Result<(NodeResourceUsageAndCost<'a>, NodeCostTerms), AnalyticalCostError> {
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
    // A summary build's output while its states still receive values
    // (each state open): what a query-time build holds while it runs and
    // what an ingestion-time chain's current window holds.
    let mut open_output_bytes: Option<u64> = None;
    // The narrower states a `SummaryMergeWindows` merges per execution.
    let mut narrower_states_merged_per_execution: u64 = 0;
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
    // A cover's mean windows per level ([`cover_windows_per_level`]).
    let cover_windows =
        if let Payload::NonASAP(NonASAPOp::TimeRangeCover { range, .. }) = &node.payload {
            Some(cover_windows_per_level(range, producers, evaluation_grid))
        } else {
            None
        };
    // A cover's is its mean over the evaluation times it runs at, not a
    // whole number.
    let tumbling_windows: f64 = match &node.payload {
        Payload::NonASAP(NonASAPOp::TimeRange { range, .. })
            if chain_tumbling_window_width_ms(producers).is_some() =>
        {
            selected_tumbling_windows(range, producers)? as f64
        }
        Payload::NonASAP(NonASAPOp::TimeRangeCover { .. }) => cover_windows
            .as_ref()
            .map_or(1.0, |per_level| per_level.iter().sum::<f64>())
            .max(1.0),
        payload if payload.keeps_input_window() => producers
            .first()
            .map_or(1.0, |producer| producer.tumbling_windows),
        // Any other operator spans one window: `keeps_input_window`
        // answers per operator, so listing them here would decide nothing.
        Payload::NonASAP(_) | Payload::ASAP(_) => 1.0,
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
            aggregate_lookup(table, node.output_state.timing, measures, series)
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
            | NonASAPOp::TimeRangeCover { .. }
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
            | ASAPOp::SummaryMergeWindows { .. }
            | ASAPOp::SummarySubtract { .. }
            | ASAPOp::SummaryDelete { .. }
            | ASAPOp::SummaryJoin { .. }
            | ASAPOp::Extension { .. },
        ) => lookup_at_scale(
            table,
            operator,
            node.output_state.timing,
            family,
            sketch_size,
            series,
        ),
    };
    // The kernel's measured bytes per state, when the table has them.
    let measured_state_bytes = measured_unit_cost
        .and_then(|p| p.memory_bytes_per_state)
        .map(|bytes| bytes.round() as u64);
    let mut cost_source = NodeCostSource::Analytical;
    // The undeclared data facts its cost assumed.
    let mut assumed_data_facts = Vec::new();
    // At ingestion time, the states it emits per second at window closes,
    // and the formula's CPU operations per second for emitting them.
    let mut states_emitted_per_second = 0.0;
    let mut formula_emit_ops_per_second = 0.0;
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
    // The one source column a summary built here folds
    // (`ProducerOutput::unchanged_column`; `None` for computed values), and
    // what the sources declare about its values: whether every one is
    // non-negative, and their magnitude range.
    let folded_values = |update: &SummaryUpdate| {
        let column = producers
            .first()
            .and_then(|producer| producer.unchanged_column(&update.weight));
        let (non_negative, range) = column.map_or((false, None), |column| {
            let non_negative = !sources.is_empty()
                && sources
                    .iter()
                    .all(|source| declared_non_negative(data, source, column));
            let range = declared_value_magnitude_range(data, sources.iter().copied(), column);
            (non_negative, range)
        });
        (column, non_negative, range)
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
    // The values the column a summary built here folds takes in one state
    // (a keyed summary's item column, e.g. the `url` a top-k heap counts;
    // else its weight column): the fewer of its declared distinct values
    // and the sources' bound per output row; `None` when neither is
    // declared.
    let folded_distinct_values = |update: &SummaryUpdate| -> Option<u64> {
        let column = producers.first().and_then(|producer| {
            producer.unchanged_column(update.item.as_ref().unwrap_or(&update.weight))
        });
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
    // The magnitude range a summary built here over `input_rows` of
    // `column` is costed for when its values declare none
    // (`assumed_magnitude_range`), named with the sources that declare no
    // range for that column (every source when it folds computed values,
    // `column` `None`). None when the measured table gives its bytes, which
    // assume no range.
    let assumed_magnitude_range_fact = |column: Option<&str>, input_rows: StateInputRows| {
        if measured_state_bytes.is_some() {
            return None;
        }
        let assumed_range = assumed_magnitude_range(node.parameters.as_ref()?, input_rows)?;
        let undeclared = sources
            .iter()
            .copied()
            .filter(|&source| {
                column.is_none_or(|column| {
                    declared_value_magnitude_range(data, [source], column).is_none()
                })
            })
            .map(|source| source.to_string())
            .collect();
        Some(AssumedDataFact {
            sources: undeclared,
            assumption: DataFactAssumption::DDSketchValueMagnitudeRange {
                column: column.map(str::to_string),
                assumed_range,
            },
        })
    };
    // What a summary built here over `rows_per_state` input rows per state
    // (`None`: unbounded, at ingestion time) holds and costs per row: its
    // input rows, its bytes per state (the measured table's when it has
    // them), its update terms per row (with the labels it carries), and the
    // magnitude range it assumed.
    let summary_state_build = |family: &FieldDataType,
                               update: &SummaryUpdate,
                               reduction: &Reduction,
                               rows_per_state: Option<u64>|
     -> Result<SummaryStateBuild, AnalyticalCostError> {
        let (column, non_negative, value_magnitude_range) = folded_values(update);
        let input_rows = StateInputRows {
            rows_per_state,
            non_negative,
            value_magnitude_range,
            distinct_values: folded_distinct_values(update),
        };
        let state_bytes = measured_state_bytes.unwrap_or(summary_state_bytes(
            family,
            node.parameters.as_ref(),
            input_rows,
        )?);
        let open_state_bytes = measured_state_bytes.unwrap_or(open_summary_state_bytes(
            family,
            node.parameters.as_ref(),
            input_rows,
        )?);
        let update_terms_per_row = update_terms_per_row(
            family,
            node.parameters.as_ref(),
            input_rows.rows_per_state,
            input_rows.distinct_values,
        )
        .plus_terms(&carried_label_terms_per_row(&node.output_schema, reduction));
        Ok(SummaryStateBuild {
            input_rows,
            state_bytes,
            open_state_bytes,
            update_terms_per_row,
            magnitude_range_fact: assumed_magnitude_range_fact(column, input_rows),
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
    // The exact rate its input's raw rows arrive at, when known.
    let input_raw_rows_per_ms = producers
        .first()
        .and_then(|producer| producer.raw_rows_per_ms);
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
                let estimate = estimate_operator_with_terms(
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
                // every row's differs; at most each group's bound on
                // distinct items, and the rows read.
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
                    .chain(distinct_items_per_group().map(|per| per.saturating_mul(group_count)))
                    .chain([input.rows])
                    .min()
                    .unwrap_or(input.rows);
                let terms_per_row = hash_aggregate_terms_per_row(
                    keys,
                    measures,
                    input.rows,
                    group_count,
                    distinct_values,
                )
                .plus_terms(&carried_label_terms_per_row(&node.output_schema, reduction));
                let operations_per_row = terms_per_row.operations();
                let group_visits = visits(group_count);
                let estimate = estimate_operator_with_terms(
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
                .map(|(estimate, operator_terms)| {
                    // The operator charges its rows times the per-row
                    // operations rounded up, without a constant: those
                    // per-row terms charge all but the rounding.
                    let terms = operator_terms
                        .attributing_to_constants(&terms_per_row.times(input.rows as f64))
                        .plus(CostConstant::OperationsPerGroupPerRawBatch, group_visits);
                    charged(terms, estimate.peak_memory_bytes(), estimate.scan_bytes())
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
                let estimate = estimate_operator_with_terms(
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
                let estimate = estimate_operator_with_terms(
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
                    estimate_operator_with_terms(
                        PhysicalOperator::TimeShift,
                        OperatorStatistics::PassThrough {
                            edges: unary(input),
                        },
                    ),
                    format!("time shift: pass {} rows, free", input.rows),
                )
            }
            NonASAPOp::TimeRangeCover { range, .. } => {
                // Each level's mean windows per evaluation, its states per
                // window each: wider levels' states are fewer and larger.
                // Every level holds one state per group per window: the
                // groups times the windows selected, in rows; the bytes by
                // each level's states.
                let per_level = cover_windows.clone().unwrap_or_default();
                let group_count = producers.iter().map(|p| p.output.rows).max().unwrap_or(1);
                let mut bytes = 0_u64;
                for (producer, windows) in producers.iter().zip(&per_level) {
                    bytes = bytes
                        .checked_add((producer.output.bytes as f64 * windows).round() as u64)
                        .ok_or(AnalyticalCostError::Overflow)?;
                }
                let out = EdgeStatistics {
                    rows: ((group_count as f64 * tumbling_windows).round() as u64).max(1),
                    bytes,
                };
                // What each selected state folds on average: the range's
                // rows of one group over the windows selected.
                built_state_input_rows = producers.last().and_then(|narrowest| {
                    let mut rows = narrowest.state_input_rows?;
                    let narrowest_width_ms = narrowest.chain_tumbling_window_width_ms?.max(1);
                    let range_ms = u64::try_from(range.as_millis()).unwrap_or(u64::MAX);
                    rows.rows_per_state = rows.rows_per_state.map(|per_window| {
                        ((per_window as f64 * (range_ms / narrowest_width_ms) as f64)
                            / tumbling_windows.max(1.0))
                        .ceil() as u64
                    });
                    Some(rows)
                });
                let estimate = estimate_operator_with_terms(
                    PhysicalOperator::TimeRange,
                    OperatorStatistics::PassThrough {
                        edges: UnaryEdgeStatistics {
                            input: out,
                            output: out,
                            promql: None,
                        },
                    },
                );
                let windows_text = per_level
                    .iter()
                    .map(|windows| format!("{windows:.2}"))
                    .collect::<Vec<_>>()
                    .join(" + ");
                (
                    out,
                    estimate,
                    format!(
                        "time range cover {range:?}: {windows_text} tumbling windows of states \
                         per level"
                    ),
                )
            }
            NonASAPOp::TimeRange { range, .. }
                if chain_tumbling_window_width_ms(producers).is_some() =>
            {
                // Over a chain, the windows inside the range: one
                // window's states per window.
                let windows = tumbling_windows as u64;
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
                let estimate = estimate_operator_with_terms(
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
                let estimate = estimate_operator_with_terms(
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
                let estimate = estimate_operator_with_terms(
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
            let SummaryStateBuild {
                input_rows,
                state_bytes,
                open_state_bytes,
                update_terms_per_row: update_terms,
                magnitude_range_fact,
            } = summary_state_build(
                family,
                update,
                reduction,
                (!ingestion).then(|| input.rows.div_ceil(group_count.max(1))),
            )?;
            built_state_input_rows = Some(input_rows);
            open_output_bytes = Some(
                group_count
                    .checked_mul(
                        open_state_bytes + state_row_bytes_beside_state(&node.output_schema),
                    )
                    .ok_or(AnalyticalCostError::Overflow)?,
            );
            assumed_data_facts.extend(magnitude_range_fact);
            let update_ops = update_terms.operations();
            let out = EdgeStatistics {
                rows: group_count,
                bytes: group_count
                    .checked_mul(state_bytes + state_row_bytes_beside_state(&node.output_schema))
                    .ok_or(AnalyticalCostError::Overflow)?,
            };
            let group_visits = visits(group_count);
            // Each group's state allocated and zeroed (a counter-matrix
            // sketch's counters), before its first row.
            let allocation_terms = state_allocation_terms(node.parameters.as_ref());
            let allocation_ops = allocation_terms.operations();
            let allocation_detail = if allocation_ops > 0.0 {
                format!(
                    ", {} ops to allocate each state",
                    operations_text(allocation_ops)
                )
            } else {
                String::new()
            };
            let terms = update_terms
                .times(input.rows as f64)
                .plus(CostConstant::OperationsPerGroupPerRawBatch, group_visits)
                .plus_terms(&allocation_terms.times(group_count as f64));
            (
                out,
                Ok(charged(terms, out.bytes, 0)),
                format!(
                    "build {} into {group_count} states: {} rows x {} ops, \
                     {group_visits:.0} group visits{allocation_detail}",
                    summary_type_name(family),
                    input.rows,
                    operations_text(update_ops)
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
            // time falls in ([`rows_per_tumbling_window`], the same rows
            // whether the chain is built at ingestion or at query time).
            let group_count = groups(reduction)?;
            let rows_per_tumbling_window = rows_per_tumbling_window(
                ChainInputRows {
                    rows: input.rows,
                    raw_rows_span_ms: input_raw_rows_span_ms,
                    raw_rows_per_ms: input_raw_rows_per_ms,
                },
                *tumbling_window_width_ms,
                ingestion,
                retained_output_count,
            );
            let SummaryStateBuild {
                input_rows,
                state_bytes,
                open_state_bytes,
                update_terms_per_row: update_terms,
                magnitude_range_fact,
            } = summary_state_build(
                family,
                update,
                reduction,
                Some(rows_per_tumbling_window.div_ceil(group_count.max(1))),
            )?;
            built_state_input_rows = Some(input_rows);
            open_output_bytes = Some(
                group_count
                    .checked_mul(
                        open_state_bytes + state_row_bytes_beside_state(&node.output_schema),
                    )
                    .ok_or(AnalyticalCostError::Overflow)?,
            );
            assumed_data_facts.extend(magnitude_range_fact);
            let update_ops = update_terms.operations();
            // The states it completes in one execution, each allocated
            // when its window opens: at ingestion time one second's share
            // of each group's window (a 10 s window: a tenth of a state per
            // group); at query time every retained window's.
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
            let range_constant = match ingestion {
                true => CostConstant::ChainOperationsPerArrivingRow,
                false => CostConstant::TimeRangeOperationsPerRow,
            };
            if ingestion {
                states_emitted_per_second = emitted_states;
                formula_emit_ops_per_second = chain_emit_terms_per_state(family)
                    .plus_terms(&state_allocation_terms(node.parameters.as_ref()))
                    .times(emitted_states)
                    .operations();
            }
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
                Ok(charged(
                    update_terms
                        .times(input.rows as f64)
                        .plus(range_constant, input.rows as f64)
                        .plus(CostConstant::OperationsPerGroupPerRawBatch, group_visits)
                        .plus_terms(
                            &chain_emit_terms_per_state(family)
                                .plus_terms(&state_allocation_terms(node.parameters.as_ref()))
                                .times(emitted_states),
                        ),
                    out.bytes,
                    0,
                )),
                format!(
                    "build {} in tumbling windows of {tumbling_window_width_ms} ms for \
                     {group_count} groups: {} rows x {} ops, {group_visits:.0} \
                     group visits, {emitted_states:.1} states emitted",
                    summary_type_name(family),
                    input.rows,
                    operations_text(update_ops)
                ),
            )
        }
        Payload::ASAP(ASAPOp::SummaryMergeWindows {
            tumbling_window_width_ms,
            ..
        }) => {
            // Each wider window, when it ends, merges the narrower windows
            // inside it, per group: at ingestion time one second's share,
            // `group_count · 1000 / narrower width` merges and
            // `group_count · 1000 / width` states emitted per second.
            let narrower = producers
                .first()
                .ok_or(AnalyticalCostError::UnsupportedCandidate)?;
            let narrower_width_ms = narrower
                .chain_tumbling_window_width_ms
                .ok_or(AnalyticalCostError::UnsupportedCandidate)?
                .max(1);
            let width_ms = (*tumbling_window_width_ms).max(1);
            let windows_per_window = (width_ms / narrower_width_ms).max(1);
            let group_count = narrower.output.rows.max(1);
            // Stage 2 offers it at ingestion time only.
            if !ingestion {
                return Err(AnalyticalCostError::UnsupportedCandidate);
            }
            let merged = group_count as f64 * ONE_SECOND_MS as f64 / narrower_width_ms as f64;
            let emitted = group_count as f64 * ONE_SECOND_MS as f64 / width_ms as f64;
            narrower_states_merged_per_execution = merged.round() as u64;
            let merged_input_rows = narrower.state_input_rows.map(|mut rows| {
                rows.rows_per_state = rows
                    .rows_per_state
                    .map(|per_window| per_window.saturating_mul(windows_per_window));
                rows
            });
            built_state_input_rows = merged_input_rows;
            let input_state_bytes = state_bytes_per_row(narrower.output, narrower.output_schema);
            let merged_state_bytes = match (family, merged_input_rows) {
                (StateFamily::Sketch(params), Some(rows)) => sketch_state_bytes(params, rows),
                _ => input_state_bytes,
            };
            let terms = merge_terms_per_state(
                family,
                input_state_bytes,
                merged_state_bytes,
                narrower.state_input_rows,
                windows_per_window,
            )
            .times(merged)
            .plus_terms(
                &node
                    .output_schema
                    .summary_state_type()
                    .map_or_else(NodeCostTerms::default, chain_emit_terms_per_state)
                    .times(emitted),
            );
            let out = EdgeStatistics {
                rows: group_count,
                bytes: group_count
                    .checked_mul(
                        merged_state_bytes + state_row_bytes_beside_state(&node.output_schema),
                    )
                    .ok_or(AnalyticalCostError::Overflow)?,
            };
            (
                out,
                Ok(charged(terms, out.bytes, 0)),
                format!(
                    "merge tumbling windows of {narrower_width_ms} ms into {width_ms} ms for \
                     {group_count} groups: {merged:.1} states merged, {emitted:.1} states emitted"
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
                // A cover's windows are a mean: its rows over that mean,
                // rounded (its rows are the groups times the mean, rounded).
                let windows = producer.tumbling_windows.max(1.0);
                group_count = group_count.max(match windows.fract() == 0.0 {
                    true => producer.output.rows.div_ceil(windows as u64),
                    false => (producer.output.rows as f64 / windows).round() as u64,
                });
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
            // A merge target receives its inputs' states: priced open.
            let merged_state_bytes = match (family, merged_input_rows) {
                (StateFamily::Sketch(params), Some(rows)) => open_sketch_state_bytes(params, rows),
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
            let merge_terms = match keeps_input_groups && states <= group_count {
                true => pass_through_merge_terms(group_count),
                false => producers
                    .iter()
                    .fold(NodeCostTerms::default(), |terms, producer| {
                        terms.plus_terms(
                            &merge_terms_per_state(
                                family,
                                input_state_bytes(producer),
                                merged_state_bytes,
                                producer.state_input_rows,
                                states.div_ceil(group_count.max(1)),
                            )
                            .times(producer.output.rows as f64),
                        )
                    })
                    .plus_terms(&merge_output_terms(family, group_count)),
            };
            let out = EdgeStatistics {
                rows: group_count,
                bytes: (merged_state_bytes + state_row_bytes_beside_state(&node.output_schema))
                    .checked_mul(group_count)
                    .ok_or(AnalyticalCostError::Overflow)?,
            };
            (
                out,
                Ok(charged(merge_terms, out.bytes, 0)),
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
                    StateLastWrittenBy::Merge {
                        input_rows: producer.state_input_rows,
                    }
                }
                producer => StateLastWrittenBy::Updates {
                    input_rows: producer.and_then(|producer| producer.state_input_rows),
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
                Ok(charged(
                    estimate_terms_per_group(family, input_state_bytes(), written_by)
                        .times(input.rows as f64),
                    0,
                    0,
                )),
                format!("estimate {} groups x {per_group}", input.rows),
            )
        }
        Payload::ASAP(ASAPOp::FinalizeExactAccumulator { .. }) => (
            edge(input.rows)?,
            // An exact accumulator's read is its output row alone.
            Ok(charged(
                NodeCostTerms::of(CostConstant::OutputRowOperationsPerGroup, input.rows as f64),
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
            Ok(charged(
                NodeCostTerms::without_constant(input.rows as f64),
                0,
                0,
            )),
            format!("{} rows", input.rows),
        ),
    };
    let (estimate, terms) = estimate?;
    let (multiplier, per_second) = rate;
    // The formula's ms of emitting states at window closes, per execution,
    // scaled by the calibration: charged whether the node is priced from
    // the formulas or from a measured entry (whose `ns_per_state_emitted`
    // adds to it).
    let formula_emit_ms_per_execution =
        formula_emit_ops_per_second * speed.ms_per_cpu_op * formula_scale;
    let formula_ms = estimate.cpu_ops() * speed.ms_per_cpu_op
        + estimate.scan_bytes() as f64 * speed.ms_per_scanned_byte;
    // What one computation processes, the unit its entry is costed per
    // ([`OperatorKind`]): the samples a scan reads, the states a merge
    // reads, otherwise the rows of its (first) input.
    let units = match &node.payload {
        Payload::NonASAP(NonASAPOp::Scan { .. }) => out.rows,
        Payload::ASAP(ASAPOp::SummaryMerge { .. }) => merged_states(producers)?,
        // The narrower states it merges in one execution (one second at
        // ingestion time).
        Payload::ASAP(ASAPOp::SummaryMergeWindows { .. }) => narrower_states_merged_per_execution,
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
            | NonASAPOp::TimeRangeCover { .. }
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
            // At ingestion time, the states emitted at window closes too:
            // the formula's emit term, plus the measured cost per state.
            let measured_ms = (units as f64 * unit.ns_per_unit
                + unit.ns_per_invocation
                + states_emitted_per_second * unit.ns_per_state_emitted)
                * 1e-6
                + formula_emit_ms_per_execution;
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
    // A node reading summary states or a chain's windows works on closed
    // windows: refresh work. One reading raw rows works per arriving row.
    let reads_closed_windows = producers.iter().any(|producer| {
        producer.family != StateFamily::None || producer.chain_tumbling_window_width_ms.is_some()
    });
    let phase_times = phase_times(
        reads_closed_windows || settings.data_plane_execution.ingest_work_at_refresh,
        ingestion,
        window_ms,
        node_ms,
        node_ms * per_second,
        (formula_emit_ms_per_execution
            + measured_unit_cost.map_or(0.0, |unit| {
                states_emitted_per_second * unit.ns_per_state_emitted * 1e-6
            }))
            * per_second,
        states_emitted_per_second * per_second * table.refresh_io_wait.ns_per_state_emitted * 1e-6,
    );

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
    // The output being built, its states still open: what a query-time
    // build holds while it runs, and the newest window an ingestion-time
    // view keeps besides its sealed ones.
    let open_state = open_output_bytes.map_or(state, |open| state.max(open));
    let (materialized_bytes, medium) = match (ingestion, node.lifetime, &node.payload) {
        (_, Lifetime::None, _) => {
            // A summary build holds its states open while it runs.
            let working_memory_bytes = match open_output_bytes {
                Some(open) => estimate.peak_memory_bytes().max(open),
                None => estimate.peak_memory_bytes(),
            };
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
                    // The newest window is built open while one runs.
                    peak_memory_bytes = between
                        .checked_add(open_state)
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
                    held_bytes = Some(open_state);
                    peak_memory_bytes = open_state;
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
                .checked_mul(retained_output_count)
                .and_then(|sealed| sealed.checked_add(open_state))
                .ok_or(AnalyticalCostError::Overflow)?,
            medium,
        ),
        // A whole state materialized across evaluations at query time: it
        // (sealed) and the one being built (open).
        (false, Lifetime::AcrossEvaluations(medium), _) => (
            state
                .checked_add(open_state)
                .ok_or(AnalyticalCostError::Overflow)?,
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
    // A scan's rows arrive at its sources' rate; a time range and the
    // operators that keep each row's window pass on their input's. The
    // rate holds while its rows are that rate over their span.
    let raw_rows_per_ms = if matches!(node.payload, Payload::NonASAP(NonASAPOp::Scan { .. })) {
        rows_per_ms
    } else if matches!(node.payload, Payload::NonASAP(NonASAPOp::TimeRange { .. }))
        || node.payload.keeps_input_window()
    {
        input_raw_rows_per_ms
    } else {
        None
    }
    .filter(|&rows_per_ms| {
        raw_rows_span_ms.is_some_and(|span_ms| {
            out.rows == ((rows_per_ms * span_ms as f64).round() as u64).max(1)
        })
    });
    let usage = NodeResourceUsageAndCost {
        output: out,
        sources,
        family,
        unchanged_columns,
        tumbling_windows,
        chain_tumbling_window_width_ms,
        state_input_rows,
        raw_rows_span_ms,
        raw_rows_per_ms,
        work_ms_per_execution: node_ms,
        cpu_ops_per_second: estimate.cpu_ops() * per_second,
        scanned_bytes_per_second: estimate.scan_bytes() as f64 * per_second,
        average_memory_bytes,
        peak_memory_bytes,
        disk_bytes,
        maintained,
        raw_scan,
        fit,
        series,
        cost: NodeCost {
            operator,
            timing: node.output_state.timing,
            cost_per_second: cost,
            cpu_core_seconds_per_second: node_ms * per_second / 1_000.0,
            multiplier,
            detail,
            cost_source,
            assumed_data_facts,
            phase_times,
            priced_outside_measured_scale: measured_unit_cost
                .is_some_and(|unit| unit.outside_measured_scale),
        },
    };
    Ok((usage, terms))
}

/// A node's time by phase ([`PhaseTimes`]): at query time, one
/// computation's `ms_per_execution`; at ingestion time, `ms_per_second` is its CPU
/// per second, of which `emit_ms_per_second` emits states at window
/// closes, waiting `emit_wait_ms_per_second` on I/O. A node reading raw
/// rows (not `reads_closed_windows`) does ingest work but for the states
/// it emits; one reading closed windows does refresh work only. A node
/// without a refresh window (`window_ms` `None`) does only ingest work.
fn phase_times(
    reads_closed_windows: bool,
    ingestion: bool,
    window_ms: Option<u64>,
    ms_per_execution: f64,
    ms_per_second: f64,
    emit_ms_per_second: f64,
    emit_wait_ms_per_second: f64,
) -> PhaseTimes {
    if !ingestion {
        return PhaseTimes {
            query_ms_per_step: ms_per_execution,
            ..PhaseTimes::default()
        };
    }
    let per_arriving_row = !reads_closed_windows;
    let Some(width_ms) = window_ms.filter(|&width| width > 0) else {
        return PhaseTimes {
            ingest_cpu_core_seconds_per_second: ms_per_second / 1_000.0,
            ..PhaseTimes::default()
        };
    };
    let refresh_ms_per_second = match per_arriving_row {
        true => emit_ms_per_second.min(ms_per_second),
        false => ms_per_second,
    };
    let windows_per_second = 1_000.0 / width_ms as f64;
    let refresh_cpu_ms_per_window = refresh_ms_per_second / windows_per_second;
    PhaseTimes {
        ingest_cpu_core_seconds_per_second: (ms_per_second - refresh_ms_per_second) / 1_000.0,
        refresh_cpu_ms_per_window,
        refresh_wall_ms_per_window: refresh_cpu_ms_per_window
            + emit_wait_ms_per_second / windows_per_second,
        refresh_window_width_ms: Some(width_ms),
        query_ms_per_step: 0.0,
    }
}

/// A summary build's per-state results shared by `SummaryAgg` and
/// `SummaryTumblingWindowChain`.
struct SummaryStateBuild {
    input_rows: StateInputRows,
    /// Bytes of one sealed state, as a stored window keeps it.
    state_bytes: u64,
    /// Bytes of one state still receiving values (a chain's current window,
    /// a query-time build before it is sealed).
    open_state_bytes: u64,
    update_terms_per_row: NodeCostTerms,
    magnitude_range_fact: Option<AssumedDataFact>,
}

/// A formula estimate whose CPU operations are `terms` at the built-in
/// values, holding `peak_memory_bytes` and scanning `scan_bytes`.
fn charged(
    terms: NodeCostTerms,
    peak_memory_bytes: u64,
    scan_bytes: u64,
) -> (ResourceEstimate, NodeCostTerms) {
    (
        ResourceEstimate::new(terms.operations(), peak_memory_bytes, scan_bytes),
        terms,
    )
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

/// The mean windows per evaluation a `TimeRangeCover` of `range` selects
/// from each of its levels, `producers` (widest first), averaged over the
/// evaluation times of `evaluation_grid` (the readers' grid) or, without
/// one on the narrowest level's grid, every step of the narrowest width
/// ([`mean_cover_windows_per_level`]). Example: 1 h over 5 min and 15 s:
/// `[11.05, 19.0]` every 15 s, `[11.2, 16.0]` every 60 s.
fn cover_windows_per_level(
    range: &std::time::Duration,
    producers: &[ProducerOutput<'_>],
    evaluation_grid: Option<EvaluationGrid>,
) -> Vec<f64> {
    let widths: Vec<u64> = producers
        .iter()
        .map(|producer| producer.chain_tumbling_window_width_ms.unwrap_or(1).max(1))
        .collect();
    let narrowest = widths.last().copied().unwrap_or(1);
    let grid = evaluation_grid
        .filter(|grid| {
            grid.period_ms > 0
                && grid.period_ms.is_multiple_of(narrowest)
                && grid.phase_ms.is_multiple_of(narrowest)
        })
        .unwrap_or(EvaluationGrid {
            period_ms: narrowest,
            phase_ms: 0,
        });
    mean_cover_windows_per_level(
        u64::try_from(range.as_millis()).unwrap_or(u64::MAX),
        grid,
        &widths,
    )
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

/// A chain's input rows ([`rows_per_tumbling_window`]).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ChainInputRows {
    /// The rows of one execution's input.
    pub(crate) rows: u64,
    /// The time they span ([`ProducerOutput::raw_rows_span_ms`]).
    pub(crate) raw_rows_span_ms: Option<u64>,
    /// The exact rate they arrive at ([`ProducerOutput::raw_rows_per_ms`]).
    pub(crate) raw_rows_per_ms: Option<f64>,
}

/// The rows one tumbling window of `width_ms` holds, for a chain built at
/// ingestion time (`ingestion`) or at query time, so that the same chain
/// folds the same rows either way. With the exact arrival rate of its
/// input's rows, when its rows are that rate over their span: that rate
/// over the window, at query time at most over the time the rows span.
/// Without it: its input's rows over the window's
/// share of their span (the rows are rounded: 667 a second for 666.67);
/// without a span, one second's rows at ingestion time, the rows over the
/// `retained_windows` at query time. At least one row. Example: 10,000
/// series every 15 s in 60 s windows: 40,000 rows, at ingestion time
/// (667 rows a second) and at query time over 300 s (200,000 rows) alike.
pub(crate) fn rows_per_tumbling_window(
    input: ChainInputRows,
    width_ms: u64,
    ingestion: bool,
    retained_windows: u64,
) -> u64 {
    let span_ms = input.raw_rows_span_ms.filter(|span_ms| *span_ms > 0);
    let window_ms = match (ingestion, span_ms) {
        (false, Some(span_ms)) => width_ms.min(span_ms),
        _ => width_ms,
    };
    let rows_per_ms = input.raw_rows_per_ms.filter(|&rows_per_ms| {
        span_ms.is_some_and(|span_ms| {
            input.rows == ((rows_per_ms * span_ms as f64).round() as u64).max(1)
        })
    });
    let rows = match (rows_per_ms, span_ms) {
        (Some(rows_per_ms), _) => rows_per_ms * window_ms as f64,
        (None, Some(span_ms)) => input.rows as f64 * window_ms as f64 / span_ms as f64,
        (None, None) => match ingestion {
            true => input.rows as f64 * width_ms as f64 / ONE_SECOND_MS as f64,
            false => input.rows.div_ceil(retained_windows.max(1)) as f64,
        },
    };
    (rows.round() as u64).max(1)
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
fn carried_label_terms_per_row(output_schema: &Schema, reduction: &Reduction) -> NodeCostTerms {
    let label_columns = output_schema
        .fields
        .iter()
        .filter(|field| field.dtype == FieldDataType::Plain(asap_types::ir::schema::DataType::Utf8))
        .count() as u64;
    let carried = label_columns.saturating_sub(grouping_key_count(reduction));
    NodeCostTerms::of(
        CostConstant::CarriedLabelOperationsPerRowPerLabel,
        carried as f64,
    )
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

/// An operation count for a cost detail: three decimals, trailing zeros
/// dropped, so whole counts print as before ("121"), a fractional one keeps
/// its value ("304.926") and float noise does not show ("69.99993" is
/// "70").
fn operations_text(operations: f64) -> String {
    let text = format!("{operations:.3}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
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
