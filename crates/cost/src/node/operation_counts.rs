//! The CPU operations the formulas charge per unit of work: per row a time
//! range keeps, per row a hash aggregate groups, per row folded into a
//! summary state, per state merged, per group read out.
//!
//! One operation is one nanosecond on one core at the default
//! [`ResourceSpeed`](crate::ResourceSpeed) (`ms_per_cpu_op` = 1e-6), so each
//! count below is the executor's measured time for that unit, in ns. What
//! plan selection needs is their ratios: a sketch built at query time is
//! cheaper than the exact plan only when its update is cheaper than
//! keeping and sorting the same rows. A slower or faster machine scales
//! them all through `ms_per_cpu_op`; a deployment that measures its own
//! (`CostInputs::measured_operation_costs`) replaces them node by node.
//!
//! Measured on one core of an idle server with the executor at commit
//! 59365ce80, in a release build: each query-time node of a plan run as the
//! only root over warm in-memory data, its own time the sub-plan's time
//! minus its inputs', median of 5 runs. Data: 1,000 series of a
//! non-negative gauge (`ts`, `value`, the series identity) and of a counter
//! (with a `job` label), 1 h windows at 1 Hz (3,600 rows per series,
//! tumbling windows of 10 s) and at 10 Hz (36,000 rows per series, windows
//! of 60 s). Each constant's own measurement is in its doc.
//!
//! Example: `quantile_over_time(0.99, latency[1h])` at 1 Hz reads 3.6 M
//! rows. The exact plan keeps them (`TIME_RANGE_OPERATIONS_PER_ROW`, 12),
//! groups them per series (`HASH_AGGREGATE_OPERATIONS_PER_KEY` +
//! `AGGREGATE_MEASURE_OPERATIONS_PER_ROW`, 27) and sorts each series' 3,600
//! values (11.8 levels × `SORT_OPERATIONS_PER_ROW_PER_LEVEL`, 65): 104 ns per
//! row. A DDSketch built
//! from the same rows takes 12 + `DDSKETCH_UPDATE_OPERATIONS_PER_ROW` (210)
//! = 222 ns per row, about twice the exact plan, as measured (111 and 229).

use asap_types::ir::operator::AggIntent;
use asap_types::ir::schema::FieldDataType;
use asap_types::summary_type::{ExactKind, SketchParams};

use crate::StateFamily;

/// A row kept by a time range over raw rows, or a state read by a time
/// range over a chain's tumbling windows: 11–12 ns per row over 3.6 M and
/// 36 M rows, 8–18 ns per state over 360,000 states.
pub(crate) const TIME_RANGE_OPERATIONS_PER_ROW: u64 = 12;

/// Per input row, for each grouping key of a hash aggregate: hashing the
/// key and finding its group. With `AGGREGATE_MEASURE_OPERATIONS_PER_ROW`,
/// a per-series `max_over_time` (one key, one measure) groups a row in
/// 27 ns.
pub(crate) const HASH_AGGREGATE_OPERATIONS_PER_KEY: u64 = 15;

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
/// count, min, max): an exact Max accumulator built per series at query
/// time takes 40 ns per row.
pub(crate) const EXACT_UPDATE_OPERATIONS_PER_ROW: f64 = 40.0;

/// One row folded into an exact accumulator of a counter's increase (rate,
/// increase, irate), which also tracks resets and the first and last
/// samples: 84 ns per row at query time, 84–89 at ingestion time. A hash
/// aggregate's measure over a series' samples in time order (rate, delta,
/// deriv, ...) is charged the same: it does the same work per row, and the
/// executor implements no such measure to time.
pub(crate) const COUNTER_UPDATE_OPERATIONS_PER_ROW: f64 = 84.0;

/// One counter update of any other sketch, per counter its row updates
/// (`state_bytes::summary_shape`: Count-Min and Count Sketch one per row of
/// depth, plus the heap; one for HyperLogLog and KMV), above
/// `EXACT_UPDATE_OPERATIONS_PER_ROW`, the executor's cost of handing a row
/// to its group's state. Not measured on the executor: `asap_sketchlib`'s
/// own Count-Min Sketch of depth 3 with a top-k heap updates in about
/// 72 ns, 18 ns per counter.
pub(crate) const SKETCH_COUNTER_UPDATE_OPERATIONS: f64 = 18.0;

/// One exact accumulator state merged into its group's (sum, count, min,
/// max): 128–265 ns per state over 360,000 states.
pub(crate) const EXACT_MERGE_OPERATIONS_PER_STATE: f64 = 230.0;

/// One counter-increase accumulator state merged (rate, increase, irate):
/// 346 ns per state over 360,000 states.
pub(crate) const COUNTER_MERGE_OPERATIONS_PER_STATE: f64 = 350.0;

/// One DDSketch state merged: 1.8 µs per state of 10 rows, 2.5 µs per
/// state of 600 rows.
pub(crate) const DDSKETCH_MERGE_OPERATIONS_PER_STATE: f64 = 2_500.0;

/// One KLL state merged: 5.4 µs per state of 10 rows, 8.8 µs per state of
/// 600 rows.
pub(crate) const KLL_MERGE_OPERATIONS_PER_STATE: f64 = 7_000.0;

/// Per byte of any other sketch state merged, above
/// `EXACT_MERGE_OPERATIONS_PER_STATE`: merging adds every counter of the
/// input state. Not measured on the executor: `asap_sketchlib` merges a
/// Count-Min Sketch of 16 KB in about 6 µs and a HyperLogLog of 16 KB in
/// about 13 µs, 0.4–0.8 ns per byte.
pub(crate) const SKETCH_MERGE_OPERATIONS_PER_BYTE: f64 = 1.0;

/// One group's answer read from a KLL state: 19–79 µs per group of states
/// folding 3,600 rows, 35 µs folding 36,000.
pub(crate) const KLL_ESTIMATE_OPERATIONS_PER_GROUP: f64 = 35_000.0;

/// One group's answer read from any other sketch state: 0–2.6 µs per
/// DDSketch quantile, within the measurement's resolution (±3 ms per 1,000
/// groups); `asap_sketchlib`'s own DDSketch quantile takes 0.15 µs and a
/// Count-Min Sketch top-k query 0.5–8 µs.
pub(crate) const SKETCH_ESTIMATE_OPERATIONS_PER_GROUP: f64 = 1_000.0;

/// One exact accumulator finalized into its group's value: reading one
/// state, charged as folding one row into it
/// (`EXACT_UPDATE_OPERATIONS_PER_ROW`); measured within the resolution
/// above, so it costs no more than a hash aggregate's measure producing the
/// same value.
pub(crate) const FINALIZE_OPERATIONS_PER_GROUP: f64 = EXACT_UPDATE_OPERATIONS_PER_ROW;

/// Per state a tumbling-window chain completes and emits at the end of each
/// window, beyond its rows' updates: fitting each window's time to its rows
/// and its 1,000 states at 1 Hz (10 s windows of 10,000 rows) and at
/// 10 Hz (60 s windows of 600,000 rows), 1.9 µs per exact Rate state,
/// 2.8 µs per KLL state and 3.4 µs per DDSketch state.
pub(crate) const CHAIN_EMIT_OPERATIONS_PER_STATE: f64 = 2_500.0;

/// The operations one input row costs folded into a state of `summary_type`
/// with `parameters`, whose kernel updates `counter_updates` counters per
/// row (`state_bytes::summary_shape`'s first value).
pub(crate) fn update_operations_per_row(
    summary_type: &FieldDataType,
    parameters: Option<&SketchParams>,
    counter_updates: f64,
) -> f64 {
    match (summary_type, parameters) {
        (FieldDataType::ExactAggregate(kind, _), _) if is_counter_increase(kind) => {
            COUNTER_UPDATE_OPERATIONS_PER_ROW
        }
        (FieldDataType::ExactAggregate(..), _) => EXACT_UPDATE_OPERATIONS_PER_ROW,
        (_, Some(SketchParams::DDSketch { .. })) => DDSKETCH_UPDATE_OPERATIONS_PER_ROW,
        (_, Some(SketchParams::Kll { .. })) => KLL_UPDATE_OPERATIONS_PER_ROW,
        _ => EXACT_UPDATE_OPERATIONS_PER_ROW + SKETCH_COUNTER_UPDATE_OPERATIONS * counter_updates,
    }
}

/// The operations one input state of `family` holding `state_bytes` costs
/// merged into its group's state.
pub(crate) fn merge_operations_per_state(family: StateFamily<'_>, state_bytes: u64) -> f64 {
    match family {
        StateFamily::Exact(kind) if is_counter_increase(kind) => COUNTER_MERGE_OPERATIONS_PER_STATE,
        StateFamily::Exact(_) | StateFamily::None => EXACT_MERGE_OPERATIONS_PER_STATE,
        StateFamily::Sketch(SketchParams::DDSketch { .. }) => DDSKETCH_MERGE_OPERATIONS_PER_STATE,
        StateFamily::Sketch(SketchParams::Kll { .. }) => KLL_MERGE_OPERATIONS_PER_STATE,
        StateFamily::Sketch(_) => {
            EXACT_MERGE_OPERATIONS_PER_STATE + SKETCH_MERGE_OPERATIONS_PER_BYTE * state_bytes as f64
        }
    }
}

/// The operations reading one group's answer from a state of `family`
/// costs (an exact accumulator's: finalizing it).
pub(crate) fn estimate_operations_per_group(family: StateFamily<'_>) -> f64 {
    match family {
        StateFamily::Sketch(SketchParams::Kll { .. }) => KLL_ESTIMATE_OPERATIONS_PER_GROUP,
        StateFamily::Sketch(_) => SKETCH_ESTIMATE_OPERATIONS_PER_GROUP,
        StateFamily::Exact(_) | StateFamily::None => FINALIZE_OPERATIONS_PER_GROUP,
    }
}

/// The operations one input row costs a hash aggregate's `measure` over
/// groups of `rows_per_group` rows: an exact quantile keeps the row and
/// sorts its group (`log2(rows_per_group)` levels); a measure over a
/// series' samples in time order costs a counter-increase update; any
/// other folds the value.
pub(crate) fn aggregate_measure_operations_per_row(
    measure: &AggIntent,
    rows_per_group: u64,
) -> f64 {
    match measure {
        AggIntent::Quantile { .. } => {
            AGGREGATE_MEASURE_OPERATIONS_PER_ROW
                + (rows_per_group.max(2) as f64).log2() * SORT_OPERATIONS_PER_ROW_PER_LEVEL
        }
        AggIntent::Rate
        | AggIntent::IRate
        | AggIntent::Increase
        | AggIntent::Changes
        | AggIntent::Delta
        | AggIntent::IDelta
        | AggIntent::Deriv
        | AggIntent::Resets
        | AggIntent::PredictLinear { .. } => COUNTER_UPDATE_OPERATIONS_PER_ROW,
        _ => AGGREGATE_MEASURE_OPERATIONS_PER_ROW,
    }
}

/// Whether an exact accumulator of `kind` tracks a counter's increase.
fn is_counter_increase(kind: &ExactKind) -> bool {
    matches!(
        kind,
        ExactKind::Rate | ExactKind::Increase | ExactKind::IRate
    )
}
