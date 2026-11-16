//! The CPU operations the formulas charge per unit of work: per row a time
//! range keeps, per row a hash aggregate groups, per row folded into a
//! summary state, per row and label column a grouping carries, per state
//! merged and per byte of a KLL state merged or read, per output group of a
//! merge, per group read out and per output row built.
//!
//! One CPU operation is one nanosecond of the reference executor's work on
//! one core: each count below is the executor's measured time for that
//! unit, in ns, and [`ResourceSpeed::ms_per_cpu_op`](crate::ResourceSpeed)
//! (1e-6 by default) turns operations into milliseconds on the deployment's
//! machine. What plan selection needs is their ratios: a sketch built at
//! query time is cheaper than the exact plan only when its update is
//! cheaper than keeping and sorting the same rows. These are the built-in
//! fallback; a table measured on the deployment
//! (`CostInputs::measured_operation_costs`) replaces them node by node.
//!
//! Operators not measured here still charge an abstract 1 operation per
//! unit, unscaled: the source scan (1 per sample plus 0.1 per byte), a
//! filter or projection (1 per row per predicate or expression), sorts,
//! joins, deduplication, set operations and the other non-summary
//! operators (`analytical`), and every node `usage` prices by its input
//! rows alone.
//!
//! Measured on one core of an idle server with the executor at commit
//! 59365ce80, in a release build: each query-time node of a plan run as the
//! only root over warm in-memory data, its own time the sub-plan's time
//! minus its inputs', median of 5 runs. Data: 1,000 series of a
//! non-negative gauge (`ts`, `value`, the series identity) and of a counter
//! (with a `job` label), 1 h windows at 1 Hz (3,600 rows per series,
//! tumbling windows of 10 s) and at 10 Hz (36,000 rows per series, windows
//! of 60 s). Each constant's own measurement is in its doc. On a host
//! with most cores busy, the sketch constants measure about twice as high
//! (DDSketch update 325–403 ns per row, merge 3.4–3.9 µs per state) while
//! the row-streaming ones (time range, hash aggregate, exact accumulators)
//! barely move: memory and cache contention slows sketch kernels most, which
//! only favours the exact plan further.
//!
//! Example: `quantile_over_time(0.99, latency[1h])` at 1 Hz reads 3.6 M
//! rows. The exact plan keeps them (`TIME_RANGE_OPERATIONS_PER_ROW`, 12),
//! groups them per series (`HASH_AGGREGATE_OPERATIONS_PER_KEY` +
//! `AGGREGATE_MEASURE_OPERATIONS_PER_ROW`, 27) and sorts each series' 3,600
//! values (11.8 levels × `SORT_OPERATIONS_PER_ROW_PER_LEVEL`, 65): 104 ns per
//! row. A DDSketch built from the same rows takes 12 +
//! `DDSKETCH_UPDATE_OPERATIONS_PER_ROW` (210) = 222 ns per row, about twice
//! the exact plan, as measured (111 and 229).

// Each rule lists every variant, as in `usage`: a wildcard arm would give a
// new operator, summary type or sketch a default cost instead of failing
// the build (under `-D warnings`).
#![warn(
    clippy::wildcard_enum_match_arm,
    clippy::match_wildcard_for_single_variants
)]

use asap_types::ir::operator::AggIntent;
use asap_types::ir::schema::FieldDataType;
use asap_types::summary_type::SketchParams;

use crate::StateFamily;

/// A row kept by a time range over raw rows, or a state read by a time
/// range over a chain's tumbling windows: 11–12 ns per row over 3.6 M and
/// 36 M rows, 8–18 ns per state over 360,000 states. A query-time tumbling
/// window chain pays it too, per raw row it reads: it keeps each row of
/// its scan's lookback in its window's range, as a time range does.
pub(crate) const TIME_RANGE_OPERATIONS_PER_ROW: f64 = 12.0;

/// Per input row, for each grouping key of a hash aggregate: hashing the
/// key and finding its group. With `AGGREGATE_MEASURE_OPERATIONS_PER_ROW`,
/// a per-series `max_over_time` (one key, one measure) groups a row in
/// 27 ns.
pub(crate) const HASH_AGGREGATE_OPERATIONS_PER_KEY: f64 = 15.0;

/// Per input row, for each plain measure of a hash aggregate (sum, count,
/// min, max, ...): folding the row's value into its group's.
pub(crate) const AGGREGATE_MEASURE_OPERATIONS_PER_ROW: f64 = 12.0;

/// Per input row, per level of the sort that an exact quantile runs over
/// its group's values (`log2` of the rows per group levels): a per-series
/// exact quantile costs 72 ns per row more than a per-series max at 3,600
/// rows per series (11.8 levels, 6.1 ns per level) and 69–97 ns more at
/// 36,000 (15.1 levels, 4.5–6.4 ns per level).
pub(crate) const SORT_OPERATIONS_PER_ROW_PER_LEVEL: f64 = 5.5;

/// One row folded into a DDSketch state: 217 ns per row at 1 Hz and 203 at
/// 10 Hz at query time, 211 at ingestion time.
pub(crate) const DDSKETCH_UPDATE_OPERATIONS_PER_ROW: f64 = 210.0;

/// One row folded into a KLL state: 106 ns per row at 1 Hz and 84 at
/// 10 Hz at query time, 94 at ingestion time.
pub(crate) const KLL_UPDATE_OPERATIONS_PER_ROW: f64 = 95.0;

/// One row folded into an exact accumulator that keeps one value (sum,
/// count, min, max), beyond any label it carries
/// (`CARRIED_LABEL_OPERATIONS_PER_ROW_PER_LABEL`): over 1,000 series, an exact Max
/// built per series takes 35 ns per row at query time (median of 12
/// runs, 300,000 to 36 M rows) and 44 at ingestion time, an exact Sum
/// grouped by `job` 30 at query time.
pub(crate) const EXACT_UPDATE_OPERATIONS_PER_ROW: f64 = 40.0;

/// One row folded into an exact accumulator of a counter's increase (rate,
/// increase, irate), which also tracks resets and the first and last
/// samples, beyond any label it carries: built per series carrying a
/// `job` label it takes 77 ns per row at query time and 87 at ingestion
/// time, of which the label is `CARRIED_LABEL_OPERATIONS_PER_ROW_PER_LABEL`
/// (37).
pub(crate) const COUNTER_UPDATE_OPERATIONS_PER_ROW: f64 = 45.0;

/// Per input row, for each label column a per-series state or group
/// carries beside its key (the `job` of a per-series sum that a later
/// `sum by (job)` reads): copying the label into the group. A per-series
/// exact `Aggregate` carrying `job` takes 51–77 ns per row where one
/// carrying nothing takes 18–38 at the same settings, and an exact Count
/// accumulator carrying it 79 ns where a Max carrying nothing takes 42:
/// 37 ns per row, the median difference.
pub(crate) const CARRIED_LABEL_OPERATIONS_PER_ROW_PER_LABEL: f64 = 37.0;

/// One counter update of any other sketch, per counter its row updates
/// ([`counters_updated_per_row`]), above
/// `EXACT_UPDATE_OPERATIONS_PER_ROW`, the executor's cost of handing a row
/// to its group's state. Not measured on the executor: `asap_sketchlib`'s
/// own Count-Min Sketch of depth 3 with a top-k heap updates in about
/// 72 ns, 18 ns per counter.
pub(crate) const SKETCH_UPDATE_OPERATIONS_PER_COUNTER: f64 = 18.0;

/// One exact accumulator state merged into its group's (sum, count, min,
/// max): 128–265 ns per state over 360,000 states.
pub(crate) const EXACT_MERGE_OPERATIONS_PER_STATE: f64 = 230.0;

/// One counter-increase accumulator state merged (rate, increase, irate):
/// 346 ns per state over 360,000 states.
pub(crate) const COUNTER_MERGE_OPERATIONS_PER_STATE: f64 = 350.0;

/// One DDSketch state merged: 1.8 µs per state of 10 rows, 2.5 µs per
/// state of 600 rows.
pub(crate) const DDSKETCH_MERGE_OPERATIONS_PER_STATE: f64 = 2_500.0;

/// One KLL state merged, beyond its bytes: the executor merges each input
/// state into a copy of its group's state, so a merge costs per byte of
/// the input state (`KLL_MERGE_OPERATIONS_PER_INPUT_BYTE`) and per byte of
/// the merged state (`KLL_MERGE_OPERATIONS_PER_MERGED_BYTE`), which grows
/// towards its `k`. The bytes are those the planner charges for each state
/// (`sketch_state_bytes` of the rows it folds), so the fit is over the
/// quantity it is applied to. Fitted over `asap_sketchlib`'s KLL at k = 200,
/// 547 and 1,404, states of 10 to 3,000 rows, 1 to 360 states per group
/// (1,000 groups, median of 3 runs), minimizing the squared log error:
/// within 2x of 20 of the 24 points, 0.41–2.4x. Example: merging 360
/// states of 10 rows (872 B each) per group, charged 17.0 KB merged at
/// k = 547 and 33.5 KB at k = 1,404 (the library holds 8.9 and 33.5 KB),
/// is charged 7.7 and 14.9 µs per state, measured 4.3 and 13.0 (4.7 and
/// 13.3 in the executor): the charge past `k` is the item array's
/// capacity, so the smaller `k` is over-charged and the two differ by
/// 1.9x where they measure 3x.
pub(crate) const KLL_MERGE_OPERATIONS_PER_STATE: f64 = 360.0;

/// Per byte of a KLL input state merged (see
/// `KLL_MERGE_OPERATIONS_PER_STATE`).
pub(crate) const KLL_MERGE_OPERATIONS_PER_INPUT_BYTE: f64 = 0.1;

/// Per byte of the merged KLL state each input state is merged into (see
/// `KLL_MERGE_OPERATIONS_PER_STATE`).
pub(crate) const KLL_MERGE_OPERATIONS_PER_MERGED_BYTE: f64 = 0.43;

/// Per output group of a `SummaryMerge`, beyond its input states' merges:
/// creating the group's state and its output row. Merging exact
/// accumulators over 1,000 groups costs 1.17 µs per group with one state
/// per group and 215 ns per state plus about 0.95 µs per group with 5 to
/// 360 (median of 20–26 runs per point). Less than a finalize's or an
/// estimate's output row (`OUTPUT_ROW_OPERATIONS_PER_GROUP`, 2,500): a
/// merge's row takes its group's key columns and merged state as they are,
/// where a finalize or an estimate also reads each state into a new value
/// column.
const MERGE_OPERATIONS_PER_OUTPUT_GROUP: f64 = 950.0;

/// Per byte of any other sketch state merged, above
/// `EXACT_MERGE_OPERATIONS_PER_STATE`: merging adds every counter of the
/// input state. Not measured on the executor: `asap_sketchlib` merges a
/// Count-Min Sketch of 16 KB in about 6 µs and a HyperLogLog of 16 KB in
/// about 13 µs, 0.4–0.8 ns per byte.
pub(crate) const SKETCH_MERGE_OPERATIONS_PER_BYTE: f64 = 1.0;

/// Per byte of the KLL state one group's answer is read from, before its
/// output row: the quantile sorts the items the state holds. The bytes are
/// those the planner charges for the state (`sketch_state_bytes`); 0.9 per
/// byte fits `asap_sketchlib`'s KLL at k = 200 to 1,404 over the same 24
/// merged states as `KLL_MERGE_OPERATIONS_PER_STATE` (median of 3 runs over
/// 1,000 groups) within 2x at 21 of them. Example: a merged state of 360
/// states of 10 rows is charged 15 µs at k = 547 (17.0 KB) and 30 µs at
/// k = 1,404 (33.5 KB), measured 23 and 42.
pub(crate) const KLL_ESTIMATE_OPERATIONS_PER_BYTE: f64 = 0.9;

/// One group's answer read from any other sketch state, before its output
/// row (`OUTPUT_ROW_OPERATIONS_PER_GROUP`): a DDSketch quantile estimate
/// measures 3.7 µs per group with its row (median of 11 nodes of 1,000
/// groups); `asap_sketchlib`'s own DDSketch quantile takes 0.15 µs and a
/// Count-Min Sketch top-k query 0.5–8 µs.
pub(crate) const SKETCH_ESTIMATE_OPERATIONS_PER_GROUP: f64 = 1_000.0;

/// One group's output row built from its value, with the group's labels
/// and series identity, by a hash aggregate, a finalize or an estimate (a
/// top-k estimate emits `k` rows per group and is charged one): a
/// finalize of exact accumulators measures 2.5 µs
/// per group (median over 135 nodes of 1,000 groups each, 1.8–15 µs). A
/// per-series hash aggregate builds the same rows: e.g. a per-series max
/// over 300,000 rows into 1,000 groups measures 11.3 ms, 8.1 ms of it its
/// rows' work (`hash_aggregate_operations_per_row`, 27 per row), about
/// 3 µs per group.
pub(crate) const OUTPUT_ROW_OPERATIONS_PER_GROUP: f64 = 2_500.0;

/// Per state a tumbling-window chain completes and emits at the end of each
/// window, beyond its rows' updates: fitting each window's time to its rows
/// and its 1,000 states at 1 Hz (10 s windows of 10,000 rows) and at
/// 10 Hz (60 s windows of 600,000 rows), 1.9 µs per exact Rate state,
/// 2.8 µs per KLL state and 3.4 µs per DDSketch state. Measured apart from
/// `OUTPUT_ROW_OPERATIONS_PER_GROUP` and equal to it: both build one row per
/// group with its labels and series identity, here around a state, there
/// around a value.
pub(crate) const CHAIN_EMIT_OPERATIONS_PER_STATE: f64 = 2_500.0;

/// The operations one input row costs folded into a state of `summary_type`
/// with `parameters`.
pub(crate) fn update_operations_per_row(
    summary_type: &FieldDataType,
    parameters: Option<&SketchParams>,
) -> f64 {
    match (summary_type, parameters) {
        (FieldDataType::ExactAggregate(kind, _), _) if kind.tracks_counter_increase() => {
            COUNTER_UPDATE_OPERATIONS_PER_ROW
        }
        (FieldDataType::ExactAggregate(..), _) => EXACT_UPDATE_OPERATIONS_PER_ROW,
        (_, Some(SketchParams::DDSketch { .. })) => DDSKETCH_UPDATE_OPERATIONS_PER_ROW,
        (_, Some(SketchParams::Kll { .. })) => KLL_UPDATE_OPERATIONS_PER_ROW,
        (
            _,
            Some(
                parameters @ (SketchParams::Hydra(_)
                | SketchParams::Cms { .. }
                | SketchParams::CountSketch { .. }
                | SketchParams::CmsWithHeap { .. }
                | SketchParams::CountSketchWithHeap { .. }
                | SketchParams::UnivMon { .. }
                | SketchParams::Hll { .. }
                | SketchParams::Kmv { .. }),
            ),
        ) => {
            EXACT_UPDATE_OPERATIONS_PER_ROW
                + SKETCH_UPDATE_OPERATIONS_PER_COUNTER * counters_updated_per_row(parameters)
        }
        // A summary without sketch parameters (a sample): one counter.
        (_, None) => EXACT_UPDATE_OPERATIONS_PER_ROW + SKETCH_UPDATE_OPERATIONS_PER_COUNTER,
    }
}

/// The counters one input row updates in a sketch with `parameters`: one
/// per row of depth for Count-Min and Count Sketch, plus one heap update for
/// their top-k variants; for UnivMon, the same per layer it updates
/// (`univmon_expected_layers`); one for every other sketch. Example: a
/// Count-Min Sketch of depth 3 with a heap updates 4.
pub(crate) fn counters_updated_per_row(parameters: &SketchParams) -> f64 {
    match parameters {
        SketchParams::Hydra(hydra) => counters_updated_per_row(&hydra.per_subpopulation()),
        SketchParams::Cms { depth, .. } | SketchParams::CountSketch { depth, .. } => {
            f64::from(*depth)
        }
        SketchParams::CmsWithHeap { depth, .. }
        | SketchParams::CountSketchWithHeap { depth, .. } => f64::from(*depth) + 1.0,
        SketchParams::UnivMon {
            sketch_rows,
            layers,
            ..
        } => univmon_expected_layers(*layers) * (f64::from(*sketch_rows) + 1.0),
        SketchParams::Kll { .. }
        | SketchParams::DDSketch { .. }
        | SketchParams::Hll { .. }
        | SketchParams::Kmv { .. } => 1.0,
    }
}

/// Layers a UnivMon row updates in expectation. `asap_sketchlib` updates
/// layers `0..=b`, with bottom layer `b` the index of the first zero among
/// hash bits `1..layers`, capped at `layers - 1`; so it reaches layer `j`
/// with probability `2^-j`, and the expectation is
/// `sum_{j < layers} 2^-j = 2 - 2^(1 - layers)`.
fn univmon_expected_layers(layers: u8) -> f64 {
    2.0 - 2f64.powi(1 - i32::from(layers.max(1)))
}

/// The operations one input state of `family` holding `state_bytes` costs
/// merged into its group's state, which holds `merged_state_bytes` once
/// merged. Example: a KLL state of 872 bytes merged into one of 33,512
/// costs 360 + 0.1 × 872 + 0.43 × 33,512 = 14,857.
pub(crate) fn merge_operations_per_state(
    family: StateFamily<'_>,
    state_bytes: u64,
    merged_state_bytes: u64,
) -> f64 {
    match family {
        StateFamily::Exact(kind) if kind.tracks_counter_increase() => {
            COUNTER_MERGE_OPERATIONS_PER_STATE
        }
        StateFamily::Exact(_) | StateFamily::None => EXACT_MERGE_OPERATIONS_PER_STATE,
        StateFamily::Sketch(SketchParams::DDSketch { .. }) => DDSKETCH_MERGE_OPERATIONS_PER_STATE,
        StateFamily::Sketch(SketchParams::Kll { .. }) => {
            KLL_MERGE_OPERATIONS_PER_STATE
                + KLL_MERGE_OPERATIONS_PER_INPUT_BYTE * state_bytes as f64
                + KLL_MERGE_OPERATIONS_PER_MERGED_BYTE * merged_state_bytes as f64
        }
        StateFamily::Sketch(
            SketchParams::Hydra(_)
            | SketchParams::Cms { .. }
            | SketchParams::CountSketch { .. }
            | SketchParams::CmsWithHeap { .. }
            | SketchParams::CountSketchWithHeap { .. }
            | SketchParams::UnivMon { .. }
            | SketchParams::Hll { .. }
            | SketchParams::Kmv { .. },
        ) => {
            EXACT_MERGE_OPERATIONS_PER_STATE + SKETCH_MERGE_OPERATIONS_PER_BYTE * state_bytes as f64
        }
    }
}

/// The operations a `SummaryMerge` with `output_groups` groups costs
/// beyond its input states' merges ([`merge_operations_per_state`]): each
/// group's state and output row. Example: 1,000 groups cost 950,000.
pub(crate) fn merge_output_operations(output_groups: u64) -> f64 {
    output_groups as f64 * MERGE_OPERATIONS_PER_OUTPUT_GROUP
}

/// The operations reading one group's answer from a state of `family`
/// holding `state_bytes` costs, its output row included (an exact
/// accumulator's: finalizing it, the output row alone). Example: a
/// DDSketch quantile costs 1,000 + 2,500 = 3,500 per group.
pub(crate) fn estimate_operations_per_group(family: StateFamily<'_>, state_bytes: u64) -> f64 {
    let read = match family {
        StateFamily::Sketch(SketchParams::Kll { .. }) => {
            KLL_ESTIMATE_OPERATIONS_PER_BYTE * state_bytes as f64
        }
        StateFamily::Sketch(
            SketchParams::DDSketch { .. }
            | SketchParams::Hydra(_)
            | SketchParams::Cms { .. }
            | SketchParams::CountSketch { .. }
            | SketchParams::CmsWithHeap { .. }
            | SketchParams::CountSketchWithHeap { .. }
            | SketchParams::UnivMon { .. }
            | SketchParams::Hll { .. }
            | SketchParams::Kmv { .. },
        ) => SKETCH_ESTIMATE_OPERATIONS_PER_GROUP,
        StateFamily::Exact(_) | StateFamily::None => 0.0,
    };
    read + OUTPUT_ROW_OPERATIONS_PER_GROUP
}

/// The operations one input row costs a hash aggregate with
/// `grouping_key_count` keys and `measures` over groups of `rows_per_group`
/// rows: each key hashed and looked up, each measure folded (at least one
/// operation for an aggregate without measures). Example: a per-series
/// `max_over_time` (one key, one measure) costs 15 + 12 = 27.
pub(crate) fn hash_aggregate_operations_per_row(
    grouping_key_count: u64,
    measures: &[AggIntent],
    rows_per_group: u64,
) -> f64 {
    grouping_key_count as f64 * HASH_AGGREGATE_OPERATIONS_PER_KEY
        + measures
            .iter()
            .map(|measure| aggregate_measure_operations_per_row(measure, rows_per_group))
            .sum::<f64>()
            .max(1.0)
}

/// The operations one input row costs a hash aggregate's `measure` over
/// groups of `rows_per_group` rows, an estimate of its work:
/// - an exact quantile keeps the row and sorts its group
///   (`log2(rows_per_group)` levels);
/// - a counter's rate or increase, or a regression over a series' samples
///   (`deriv`, `predict_linear`), does about what the Rate accumulator does
///   per row (`COUNTER_UPDATE_OPERATIONS_PER_ROW`): it reads each sample's
///   time and value in order and keeps running terms;
/// - any other folds the value (`AGGREGATE_MEASURE_OPERATIONS_PER_ROW`),
///   including `changes`, `delta`, `idelta` and `resets`, which compare
///   neighbouring samples without tracking a counter's increase (not
///   measured).
fn aggregate_measure_operations_per_row(measure: &AggIntent, rows_per_group: u64) -> f64 {
    match measure {
        AggIntent::Quantile { .. } => {
            AGGREGATE_MEASURE_OPERATIONS_PER_ROW
                + (rows_per_group.max(2) as f64).log2() * SORT_OPERATIONS_PER_ROW_PER_LEVEL
        }
        AggIntent::Rate
        | AggIntent::IRate
        | AggIntent::Increase
        | AggIntent::Deriv
        | AggIntent::PredictLinear { .. } => COUNTER_UPDATE_OPERATIONS_PER_ROW,
        AggIntent::Count
        | AggIntent::Sum { .. }
        | AggIntent::Min { .. }
        | AggIntent::Max { .. }
        | AggIntent::Avg { .. }
        | AggIntent::StdDev { .. }
        | AggIntent::Variance { .. }
        | AggIntent::PearsonCorr { .. }
        | AggIntent::TopK { .. }
        | AggIntent::Cardinality { .. }
        | AggIntent::FrequencyL2 { .. }
        | AggIntent::FrequencyEntropy { .. }
        | AggIntent::Changes
        | AggIntent::Delta
        | AggIntent::IDelta
        | AggIntent::Resets
        | AggIntent::DoubleExpSmoothing { .. }
        | AggIntent::HistogramCount
        | AggIntent::HistogramSum
        | AggIntent::HistogramAvg
        | AggIntent::HistogramStdDev
        | AggIntent::HistogramStdVar
        | AggIntent::HistogramFraction { .. }
        | AggIntent::HistogramQuantile { .. }
        | AggIntent::Math(_)
        | AggIntent::Absent { .. }
        | AggIntent::AbsentOverTime
        | AggIntent::PresentOverTime
        | AggIntent::TimeFn(_)
        | AggIntent::Group
        | AggIntent::CountValues { .. }
        | AggIntent::LastOverTime
        | AggIntent::FirstOverTime
        | AggIntent::MadOverTime
        | AggIntent::TsOfMinOverTime
        | AggIntent::TsOfMaxOverTime
        | AggIntent::TsOfFirstOverTime
        | AggIntent::TsOfLastOverTime
        | AggIntent::Extension { .. } => AGGREGATE_MEASURE_OPERATIONS_PER_ROW,
    }
}
