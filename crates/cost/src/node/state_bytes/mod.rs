//! The bytes one summary or exact state holds and its update operations
//! per input row, from its parameters and the rows it folds.

use crate::node::analytical::AnalyticalCostError;
use crate::node::operator_statistics::EdgeStatistics;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema, PROMQL_SERIES_IDENTITY};
use asap_types::summary_type::SketchParams;
use asap_types::summary_type::KLL_MAX_K;
use asap_types::workload::ValueMagnitudeRange;

/// Bytes of one output row: each column at [`column_bytes`].
pub fn row_bytes(schema: &Schema) -> u64 {
    schema.fields.iter().map(column_bytes).sum::<u64>().max(1)
}

/// Bytes one column of an output row is priced at: a string 16 (a label
/// value, or a series identity, which raw rows share one copy of per
/// series), any other value 8. Example: a PromQL sample row of a series
/// identity, `job`, its time and value is 16 + 16 + 8 + 8 = 48 bytes.
fn column_bytes(field: &Field) -> u64 {
    match &field.dtype {
        FieldDataType::Plain(DataType::Utf8) => 16,
        _ => 8,
    }
}

/// Bytes a summary state's output row holds beside its state. A
/// per-series row (one with the series identity) holds
/// [`DataPlaneExecution::stored_row_bytes_beside_state`](crate::DataPlaneExecution)
/// (its window's time and its series key) and no label text, its labels
/// being derived from the identity; a plain column beside them other than
/// the 8-byte window time (a value) is counted at [`column_bytes`]. Any
/// other row holds its window's time and its group keys, each at
/// [`column_bytes`]. Example: a per-series Sum carrying `job` holds 72
/// bytes beside its 24-byte sealed state (at the default), 96 in all; a
/// Sum grouped by `job` holds 8 + 16 = 24.
pub(crate) fn state_row_bytes_beside_state(
    schema: &Schema,
    stored_row_bytes_beside_state: u64,
) -> u64 {
    let not_state = || {
        schema.fields.iter().filter(|field| {
            !matches!(
                field.dtype,
                FieldDataType::Sketch(..) | FieldDataType::ExactAggregate(..)
            )
        })
    };
    if !not_state().any(|field| field.name == PROMQL_SERIES_IDENTITY) {
        return not_state().map(column_bytes).sum();
    }
    // Text columns are the identity and labels derived from it; the
    // window time is in `stored_row_bytes_beside_state`.
    let other_plain_bytes: u64 = not_state()
        .filter(|field| field.dtype != FieldDataType::Plain(DataType::Utf8))
        .map(column_bytes)
        .sum();
    stored_row_bytes_beside_state + other_plain_bytes.saturating_sub(WINDOW_TIME_BYTES)
}

/// Bytes of a state row's window time, a timestamp at [`column_bytes`].
const WINDOW_TIME_BYTES: u64 = 8;

/// Bytes of the state one row of `edge` holds: its bytes per row without
/// what the row holds beside its state ([`state_row_bytes_beside_state`] of
/// the producer's `schema`). Example: 1,000 rows of stored per-series Sums
/// carrying `job`, 96,000 bytes, hold 96 − 72 = 24 bytes of state each.
pub(crate) fn state_bytes_per_row(
    edge: EdgeStatistics,
    schema: &Schema,
    stored_row_bytes_beside_state: u64,
) -> u64 {
    edge.bytes_per_row()
        .saturating_sub(state_row_bytes_beside_state(
            schema,
            stored_row_bytes_beside_state,
        ))
}

/// Exact state that keeps every value or item of its group, so grows with
/// the data; other exact accumulators are fixed-size.
pub fn keeps_every_value(measure: &AggIntent) -> bool {
    matches!(
        measure,
        AggIntent::Quantile { .. }
            | AggIntent::TopK { .. }
            | AggIntent::Cardinality { .. }
            | AggIntent::FrequencyL2 { .. }
            | AggIntent::FrequencyEntropy { .. }
    )
}

/// Bytes per state, from the summary's family, its
/// parameters and `input_rows` (the rows one state folds). The bytes are
/// `asap_sketchlib`'s `estimated_memory_bytes` for the type the executor
/// builds (Count-Min and Count Sketch over `f64` counters), given what the
/// state is assumed to hold:
/// - a Count-Min / Count Sketch top-k heap full of keys of
///   `ASSUMED_HEAP_KEY_BYTES`; each UnivMon layer's heap full of keys of
///   `ASSUMED_VALUE_KEY_BYTES`;
/// - KMV: the rows the state folds, at least one (any number when
///   unknown), each a distinct item;
/// - DDSketch: what it is expected to hold sealed, not the most it can
///   ([`ddsketch_state_bytes`]).
///
/// KLL is priced as the executor keeps a sealed state (it shrinks every
/// KLL when it seals a window, `Kll::shrink_to_fit`): the struct, the
/// 8-byte boundaries of the levels the rows fill, and the 8-byte items they
/// leave retained ([`kll_sealed_bytes`]).
///
/// An exact accumulator's state is the executor's sealed record of its
/// kind, [`ExactKind::sealed_state_bytes`].
///
/// `parameters` is the build's θ (`PhysicalASAPDAGNode::parameters`). Err
/// [`AnalyticalCostError::UnsupportedCandidate`] for a sketch without θ: a
/// planner bug, since every sketch build carries its θ.
pub fn summary_state_bytes(
    family: &FieldDataType,
    parameters: Option<&SketchParams>,
    input_rows: StateInputRows,
) -> Result<u64, AnalyticalCostError> {
    Ok(match (family, parameters) {
        (FieldDataType::Sketch(..), Some(parameters)) => sketch_state_bytes(parameters, input_rows),
        (FieldDataType::Sketch(..), None) => return Err(AnalyticalCostError::UnsupportedCandidate),
        (FieldDataType::ExactAggregate(kind, _), _) => kind.sealed_state_bytes(),
        _ => 8,
    })
}

/// What one summary state's bytes depend on besides its parameters (only
/// a KLL's, a DDSketch's and a KMV's do): the input rows it folds, the
/// sign of their values and, for a DDSketch, their magnitudes; and the
/// distinct values of the column it folds, which a KMV build's and merge's
/// CPU and the items a top-k heap holds depend on.
/// The rows are whatever its producer emits (raw data samples, rates, sums),
/// not a summary sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StateInputRows {
    /// Source rows one state folds, on average over its states, exactly
    /// (not rounded: a merge of 30 windows of 0.67 rows folds 20, not 30);
    /// [`StateInputRows::rows_per_state`] rounds it once. A state over
    /// rows another node already folded (a sum of per-series sums) counts
    /// the source rows behind them. `None` when unknown, which sizes it
    /// for every value it can be assumed to see.
    pub folded_rows_per_state: Option<f64>,
    /// Whether every value folded is proven non-negative (a source column
    /// passed on unchanged, which every source declares non-negative), so a
    /// DDSketch fills no negative store.
    pub non_negative: bool,
    /// Bounds on `|value|` of the non-zero values folded into the state;
    /// `None` when not declared, which costs a DDSketch over the assumed
    /// `DDSKETCH_ASSUMED_MAGNITUDE_RANGE`. E.g. `Some(ValueMagnitudeRange {
    /// min_abs: 0.001, max_abs: 10000.0 })` for a latency in seconds.
    pub value_magnitude_range: Option<ValueMagnitudeRange>,
    /// The distinct values the folded column takes in one state (a keyed
    /// summary's item column, else its weight column), each folded row a
    /// draw from them: the fewer of the column's declared distinct values
    /// and the sources' bound per group; `None` when neither is declared,
    /// which takes every row's value as new. E.g. `Some(1_000_000)` for
    /// `user_id` over 1,000,000 users: KMV states of 12,000 rows each then
    /// fold about 11,929 distinct users, and states merged together share
    /// some of their retained hashes. `Some(100)` for a top-k heap keyed by
    /// a `url` declaring 100 values: over 1,000 rows it holds at most 100
    /// items.
    pub distinct_values: Option<u64>,
}

impl StateInputRows {
    /// The source rows one state folds, rounded up once from
    /// [`StateInputRows::folded_rows_per_state`] (within a relative 1e-9,
    /// so 200,001.000000001 rows are 200,001), at least 1; `None` when
    /// unknown. Every formula reads the rows through this.
    pub fn rows_per_state(&self) -> Option<u64> {
        self.folded_rows_per_state
            .map(|rows| ((rows - 1e-9 * rows.abs().max(1.0)).ceil().max(1.0)) as u64)
    }

    /// Whether `other` folds what this does: the same rows once rounded,
    /// sign, magnitude range and distinct values. Candidates of one input
    /// group computing the same states fold the same.
    pub fn folds_the_same_as(&self, other: &Self) -> bool {
        self.rows_per_state() == other.rows_per_state()
            && self.non_negative == other.non_negative
            && self.value_magnitude_range == other.value_magnitude_range
            && self.distinct_values == other.distinct_values
    }

    /// What one state folds when each group merges, from every input, that
    /// input's states (`Some((rows each folds, states per group))`, the
    /// states per group exact, not rounded): their rows summed,
    /// non-negative when all are, within the magnitude range
    /// spanning all of theirs, of the most distinct values any declares.
    /// `None` when any input's is unknown. Example:
    /// 360 states of 10 rows and 60 of 100 merge into one of 9,600 rows.
    pub(crate) fn merged(inputs: impl IntoIterator<Item = Option<(Self, f64)>>) -> Option<Self> {
        let mut merged: Option<Self> = None;
        for (rows, states) in inputs.into_iter().collect::<Option<Vec<_>>>()? {
            let input = Self {
                folded_rows_per_state: rows
                    .folded_rows_per_state
                    .map(|per_state| per_state * states),
                ..rows
            };
            merged = Some(match merged {
                None => input,
                Some(sum) => Self {
                    folded_rows_per_state: sum
                        .folded_rows_per_state
                        .zip(input.folded_rows_per_state)
                        .map(|(a, b)| a + b),
                    non_negative: sum.non_negative && input.non_negative,
                    value_magnitude_range: sum
                        .value_magnitude_range
                        .zip(input.value_magnitude_range)
                        .map(|(a, b)| ValueMagnitudeRange {
                            min_abs: a.min_abs.min(b.min_abs),
                            max_abs: a.max_abs.max(b.max_abs),
                        }),
                    distinct_values: sum
                        .distinct_values
                        .zip(input.distinct_values)
                        .map(|(a, b)| a.max(b)),
                },
            });
        }
        merged
    }

    /// Unknown rows of unknown sign: the most a state is assumed to hold.
    pub const UNKNOWN: Self = Self {
        folded_rows_per_state: None,
        non_negative: false,
        value_magnitude_range: None,
        distinct_values: None,
    };
}

/// [`sketch_state_bytes`] of a sketch still receiving values (the current
/// window of a chain, a query-time build before it is sealed, a merge
/// target): a DDSketch's stores as they grow
/// ([`ddsketch_open_state_bytes`]); every other family as sealed.
pub fn open_sketch_state_bytes(params: &SketchParams, input_rows: StateInputRows) -> u64 {
    match params {
        SketchParams::DDSketch { alpha } if *alpha > 0.0 && *alpha < 1.0 => {
            ddsketch_open_state_bytes(*alpha, input_rows)
        }
        SketchParams::Hydra(hydra) => {
            open_sketch_state_bytes(&hydra.per_subpopulation(), input_rows)
        }
        _ => sketch_state_bytes(params, input_rows),
    }
}

/// [`summary_state_bytes`] of a state still receiving values
/// ([`open_sketch_state_bytes`]).
pub fn open_summary_state_bytes(
    family: &FieldDataType,
    parameters: Option<&SketchParams>,
    input_rows: StateInputRows,
) -> Result<u64, AnalyticalCostError> {
    match (family, parameters) {
        (FieldDataType::Sketch(..), Some(parameters)) => {
            Ok(open_sketch_state_bytes(parameters, input_rows))
        }
        _ => summary_state_bytes(family, parameters, input_rows),
    }
}

/// [`summary_state_bytes`] of one sketch. With [`StateInputRows::UNKNOWN`], the bytes are also the
/// size the measured table interpolates between
/// (`crate::measured`).
pub fn sketch_state_bytes(params: &SketchParams, input_rows: StateInputRows) -> u64 {
    use asap_sketchlib::{
        Classic, CountMinF64, CountMinSketchWithHeap, CountSketch, CountSketchWithHeap, DataInput,
        DdSketch, HyperLogLog, Kmv, RegularPath, UnivMon, Vector2D,
    };
    let heap_key = DataInput::Bytes(&[0; ASSUMED_HEAP_KEY_BYTES]);
    let rows = input_rows.rows_per_state().map_or(usize::MAX, |rows| {
        usize::try_from(rows.max(1)).unwrap_or(usize::MAX)
    });
    match params {
        SketchParams::Hydra(hydra) => sketch_state_bytes(&hydra.per_subpopulation(), input_rows),
        SketchParams::Cms { width, depth } => {
            CountMinF64::estimated_memory_bytes(*depth as usize, *width as usize) as u64
        }
        SketchParams::CountSketch { width, depth } => {
            CountSketch::<Vector2D<f64>, RegularPath>::estimated_memory_bytes(
                *depth as usize,
                *width as usize,
            ) as u64
        }
        SketchParams::CmsWithHeap {
            width,
            depth,
            heap_size,
        } => CountMinSketchWithHeap::<Vector2D<f64>, RegularPath>::estimated_memory_bytes(
            *depth as usize,
            *width as usize,
            *heap_size as usize,
            &heap_key,
        ) as u64,
        SketchParams::CountSketchWithHeap {
            width,
            depth,
            heap_size,
        } => CountSketchWithHeap::<Vector2D<f64>, RegularPath>::estimated_memory_bytes(
            *depth as usize,
            *width as usize,
            *heap_size as usize,
            &heap_key,
        ) as u64,
        SketchParams::Kll { k, .. } => kll_sealed_bytes(*k, input_rows),
        // The executor builds no DDSketch outside α in (0, 1), where
        // sketchlib's estimate panics: priced as an empty one.
        SketchParams::DDSketch { alpha } if !(*alpha > 0.0 && *alpha < 1.0) => {
            std::mem::size_of::<DdSketch>() as u64
        }
        // Over the declared magnitude range, or a wide spread within the
        // one assumed when none is declared (`assumed_magnitude_range`).
        SketchParams::DDSketch { alpha } => ddsketch_state_bytes(*alpha, input_rows),
        SketchParams::Hll { precision, .. } => {
            HyperLogLog::<Classic>::estimated_memory_bytes(usize::from(*precision)) as u64
        }
        SketchParams::Kmv { k, .. } => Kmv::estimated_memory_bytes(*k as usize, rows) as u64,
        SketchParams::UnivMon {
            heap_size,
            sketch_rows,
            sketch_cols,
            layers,
            ..
        } => UnivMon::estimated_memory_bytes(
            *heap_size as usize,
            *sketch_rows as usize,
            *sketch_cols as usize,
            usize::from(*layers),
            &DataInput::Bytes(&[0; ASSUMED_VALUE_KEY_BYTES]),
        ) as u64,
    }
}

/// Bytes a KLL state of `k` holds after folding `input_rows`: the struct,
/// `levels + 1` 8-byte level boundaries for the `L(n)` levels `n` rows fill
/// (`kll_levels_after`), and its 8-byte items. Up to `k` rows nothing
/// compacts, and the state is priced as the executor keeps it sealed
/// (`Kll::shrink_to_fit`): exactly the `n` items. Past `k`, the items are
/// the buffer an open sketch grows to (16 items doubled up to the
/// capacity of all its levels, `kll_buffer_items`), what a query-time merge
/// holds while it folds windows; a sealed state past `k` holds about half
/// of that, which this leaves uncharged. A state is assumed to fold at
/// least one row, as a DDSketch's is; unknown rows fill every level.
///
/// Example: `k` = 132 over a 15 s window of one sample a second (15 rows):
/// 248 + 8 · 2 + 8 · 15 = 384 bytes.
fn kll_sealed_bytes(k: u32, input_rows: StateInputRows) -> u64 {
    let k = u64::from(k.clamp(8, KLL_MAX_K));
    let (levels, items) = match input_rows.rows_per_state() {
        None => (KLL_MAX_LEVELS as u64, kll_capacity_items(k, KLL_MAX_LEVELS)),
        Some(rows) => {
            let rows = rows.max(1);
            let items = match rows <= k {
                true => rows,
                false => kll_buffer_items(k, rows),
            };
            (kll_levels_after(k, rows), items)
        }
    };
    std::mem::size_of::<asap_sketchlib::Kll<f64>>() as u64 + 8 * (levels + 1) + 8 * items
}

/// Items an open `asap_sketchlib` KLL of `k` allocates for `rows`: its
/// item array starts empty and grows when full, to at least twice its
/// length and at least 16 items, at most the capacity of all its levels
/// ([`kll_capacity_items`]): the smallest `16 · 2^j` ≥ `rows`, capped.
fn kll_buffer_items(k: u64, rows: u64) -> u64 {
    const INITIAL_BUFFER_ITEMS: u64 = 16;
    rows.div_ceil(INITIAL_BUFFER_ITEMS)
        .next_power_of_two()
        .saturating_mul(INITIAL_BUFFER_ITEMS)
        .min(kll_capacity_items(k, KLL_MAX_LEVELS))
}

/// Levels `asap_sketchlib`'s KLL has room for (`MAX_LEVELS`).
const KLL_MAX_LEVELS: usize = 61;

/// `L(n)`: the levels a KLL of `k` holds after `n` rows, a level added once
/// the top holds `k + 1` items of weight 2^top (`asap_sketchlib`'s
/// `levels_after`). 1 up to `k` rows; 2 from `k + 1`.
fn kll_levels_after(k: u64, rows: u64) -> u64 {
    let mut levels = 1;
    while levels < KLL_MAX_LEVELS as u64
        && 1u64
            .checked_shl(levels as u32 - 1)
            .and_then(|weight| weight.checked_mul(k + 1))
            .is_some_and(|items| items <= rows)
    {
        levels += 1;
    }
    levels
}

/// The items the top `levels` levels of a KLL of `k` hold at capacity:
/// `max(ceil(k · (2/3)^i), 8)` for heights `i` from the top. Over all
/// [`KLL_MAX_LEVELS`] levels, the most items the library holds: 1,206 at
/// `k` = 269.
fn kll_capacity_items(k: u64, levels: usize) -> u64 {
    const MIN_LEVEL_CAPACITY: f64 = 8.0;
    let k = k as f64;
    let mut scale = 1.0_f64;
    let mut total = 0;
    for _ in 0..levels {
        total += (k * scale).ceil().max(MIN_LEVEL_CAPACITY) as u64;
        scale *= 2.0 / 3.0;
    }
    total
}

/// Bytes of the key one Count-Min / Count Sketch top-k heap slot holds: an
/// assumption, since the workload declares no label sizes. Such a key is a
/// label set in the executor's tuple-key encoding, a tag byte and a 4-byte
/// length before each value; this assumes three labels of 16 bytes, the
/// size [`row_bytes`] gives a string.
const ASSUMED_HEAP_KEY_BYTES: usize = 3 * (1 + 4 + 16);

/// Bytes of the key one UnivMon heap slot holds: an assumption, since the
/// workload declares no label sizes. UnivMon counts the values of one
/// column, the executor keys each by its type tag and bytes, and a label
/// value is assumed to be 16 bytes, the size [`row_bytes`] gives a string.
/// A sample value's key owns no bytes, so this bounds it too.
const ASSUMED_VALUE_KEY_BYTES: usize = 1 + 16;

/// The magnitude range a DDSketch state of `params` over `input_rows` is
/// costed for when its values declare none, the one place that default is
/// applied: [`DDSKETCH_ASSUMED_MAGNITUDE_RANGE`] for a DDSketch with α in
/// (0, 1) whose values declare no range; `None` for a declared range, an α
/// costed as an empty sketch, or any other sketch, whose bytes assume
/// none. E.g. a DDSketch at α = 0.01 over a `value` without
/// `column_magnitudes` gives `Some([1, 1e9])`.
pub fn assumed_magnitude_range(
    params: &SketchParams,
    input_rows: StateInputRows,
) -> Option<ValueMagnitudeRange> {
    match params {
        SketchParams::DDSketch { alpha }
            if *alpha > 0.0 && *alpha < 1.0 && input_rows.value_magnitude_range.is_none() =>
        {
            Some(DDSKETCH_ASSUMED_MAGNITUDE_RANGE)
        }
        _ => None,
    }
}

/// The range of value magnitudes a DDSketch is assumed to see at most when
/// its column declares no `column_magnitudes`: nine decades, e.g.
/// latencies from 1 ns to 1 s, costed as magnitudes in `[1, 1e9]` (its
/// bins depend on the ratio alone). An assumption, not workload evidence.
const DDSKETCH_ASSUMED_MAGNITUDE_RANGE: ValueMagnitudeRange = ValueMagnitudeRange {
    min_abs: 1.0,
    max_abs: 1e9,
};

/// The ratio of the largest to the smallest of `rows` values a DDSketch
/// state is charged for when the workload declares nothing about its
/// values: a wide-spread margin, the expected spread of `rows` draws of a
/// lognormal of σ = 2.5 (`exp(σ · E[range of n standard normals])`, the
/// range by Blom's approximation `2 Φ⁻¹((n − 0.375) / (n + 0.25))`),
/// capped by the assumed nine decades. Example: 10 values span a ratio of
/// 2.3e3, 100 values one of 2.7e5, 600 one of 4.8e6.
///
/// The default when the data workload declares nothing, not workload
/// evidence. It is wider than latency-like (exponential) values spread,
/// which it charges 1.5–2.0x, so that a lognormal of σ = 2 is not
/// under-charged. It stops there: at σ = 3 a 10 Hz latency quantile's
/// stored DDSketch windows, measured cheaper than the exact plan
/// (`default_cost_plan_ranking`), would be priced above it. The widest
/// spreads (Zipf integers, values log-uniform over seven decades) are
/// under-charged at a few rows per state (0.6–0.7x at 10) unless their
/// range is declared, which floors their spread (see
/// [`ddsketch_state_bytes`]).
fn wide_spread_values_magnitude_ratio(rows: u64) -> f64 {
    const SIGMA: f64 = 2.5;
    if rows <= 1 {
        return 1.0;
    }
    let n = rows as f64;
    let expected_range = 2.0 * standard_normal_quantile((n - 0.375) / (n + 0.25));
    (SIGMA * expected_range).exp()
}

/// Φ⁻¹(p), the standard normal quantile, for p in (0, 1) (Acklam's
/// rational approximation, relative error below 1.2e-9).
fn standard_normal_quantile(p: f64) -> f64 {
    const A: [f64; 6] = [
        -3.969_683_028_665_376e1,
        2.209_460_984_245_205e2,
        -2.759_285_104_469_687e2,
        1.383_577_518_672_69e2,
        -3.066_479_806_614_716e1,
        2.506_628_277_459_239,
    ];
    const B: [f64; 5] = [
        -5.447_609_879_822_406e1,
        1.615_858_368_580_409e2,
        -1.556_989_798_598_866e2,
        6.680_131_188_771_972e1,
        -1.328_068_155_288_572e1,
    ];
    const C: [f64; 6] = [
        -7.784_894_002_430_293e-3,
        -3.223_964_580_411_365e-1,
        -2.400_758_277_161_838,
        -2.549_732_539_343_734,
        4.374_664_141_464_968,
        2.938_163_982_698_783,
    ];
    const D: [f64; 4] = [
        7.784_695_709_041_462e-3,
        3.224_671_290_700_398e-1,
        2.445_134_137_142_996,
        3.754_408_661_907_416,
    ];
    let p = p.clamp(1e-300, 1.0 - 1e-16);
    let tail = |q: f64| {
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    };
    if p < 0.024_25 {
        tail((-2.0 * p.ln()).sqrt())
    } else if p > 1.0 - 0.024_25 {
        -tail((-2.0 * (1.0 - p).ln()).sqrt())
    } else {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    }
}

/// The buckets a DDSketch of relative accuracy `alpha` over `input_rows`
/// is expected to span, per store, and its stores (per sign it holds: one
/// when its values are proven non-negative or it folds one row, else two).
///
/// The span is that of the values one state folds, not of the column: the
/// wide-spread margin of its rows ([`wide_spread_values_magnitude_ratio`]),
/// capped by the column's declared range (`column_magnitudes`) or, when
/// none is declared, by the assumed range ([`assumed_magnitude_range`]);
/// without known rows, that range. A declared range also floors the span
/// at the spread of the rows log-uniform over it, `ratio^((n − 1) / (n +
/// 1))`, so values that fill it (Zipf integers, values over decades) are
/// not under-charged at a few rows per state; per-series values narrower
/// than their column (latencies of one service, levels of one series) are
/// over-charged instead, which a measured table's bytes per state corrects.
/// Buckets a ratio of `ratio` between the largest and smallest magnitude
/// spans at relative accuracy `alpha`, from a bucket boundary: at least
/// one.
fn ddsketch_buckets_spanned(alpha: f64, ratio: f64) -> u64 {
    let sketch = asap_sketchlib::DdSketch::new(alpha);
    let ratio = ratio.max(1.0);
    match (sketch.bucket_index_for(ratio), sketch.bucket_index_for(1.0)) {
        (Some(high), Some(low)) => u64::try_from(i64::from(high) - i64::from(low) + 1)
            .unwrap_or(1)
            .max(1),
        _ => 1,
    }
}

fn ddsketch_span(alpha: f64, input_rows: StateInputRows) -> (u64, u64) {
    let buckets_spanned = |ratio: f64| ddsketch_buckets_spanned(alpha, ratio);
    let stores = match (input_rows.non_negative, input_rows.rows_per_state()) {
        (true, _) | (false, Some(0 | 1)) => 1,
        (false, _) => 2,
    };
    let rows_per_store = input_rows
        .rows_per_state()
        .map(|rows| rows.div_ceil(stores).max(1));
    if rows_per_store == Some(1) {
        return (1, stores);
    }
    let column_ratio = input_rows
        .value_magnitude_range
        .unwrap_or(DDSKETCH_ASSUMED_MAGNITUDE_RANGE)
        .magnitude_ratio();
    let ratio = rows_per_store.map_or(column_ratio, |rows| {
        let capped = wide_spread_values_magnitude_ratio(rows).min(column_ratio);
        match input_rows.value_magnitude_range {
            // A declared range also floors the spread at that of `rows`
            // values log-uniform over it, `ratio^((n − 1) / (n + 1))`, so a
            // range the values do fill (Zipf integers, values over decades)
            // is not under-charged at a few rows per state.
            Some(_) => {
                let n = rows as f64;
                capped.max(column_ratio.powf((n - 1.0) / (n + 1.0)))
            }
            None => capped,
        }
    });
    (buckets_spanned(ratio), stores)
}

/// The bytes a DDSketch of relative accuracy `alpha` takes stored after
/// folding `input_rows`: the executor seals a DDSketch window into the
/// sketch's view layout (`DdSketch::to_view_bytes`, one buffer per column,
/// read in place), so its price is that layout's size, an upper bound on
/// it whatever the values within the column's range:
///
/// - an 88-byte header (scalars, count width, form, each store's offset and
///   length) and the state's 4-byte offset in its column;
/// - its counts at the narrowest width holding every count (1, 2, 4 or 8
///   bytes; no count exceeds the state's rows, so the width follows from
///   them; unknown rows take 8);
/// - the counts dense over the buckets the column's range spans (declared
///   in `column_magnitudes`, or the assumed nine decades; plus one bucket
///   for a range that does not start on a bucket boundary) per store (one
///   when its values are proven non-negative or it folds one row, else
///   two), or sparse as a 4-byte index and a count per occupied bucket, at
///   most one bucket per row, whichever is smaller;
/// - each array padded to 8 bytes.
///
/// [`ddsketch_open_state_bytes`] prices one still receiving values or
/// merging (an owned sketch with 8-byte counts). A measured table's bytes
/// per state (`memory_bytes_per_state`, fitted from a deployment's stored
/// states) replace this estimate.
///
/// How it compares with what the executor holds for a sealed state, at
/// α = 0.01, the mean of 20 series of 10 to 36,000 values each (the bands
/// `state_bytes::tests` pins; a grid over α 0.001–0.1, 1–140,000 rows and
/// 15 shapes checks it never falls below 0.99):
///
/// | values | range declared | charged ÷ held |
/// |---|---|---|
/// | exponential (latency-like) | no | 1.0–4.0 |
/// | exponential (latency-like) | the generator's [2.7e-8, 1104] | 1.0–4.7 |
/// | lognormal σ = 1 | no | 1.0–5.4 |
/// | lognormal σ = 2 | no | 1.0–3.0 |
/// | log-uniform over 7 decades | no | 1.0–2.4 |
/// | log-uniform over 7 decades | [0.001, 1e4] | 1.0–1.9 |
/// | Zipf(1.1) integers in [1, 1e6] | no | 1.02–2.8 |
/// | Zipf(1.1) integers in [1, 1e6] | [1, 1e6] | 1.0–1.9 |
/// | narrow gauge 100 ± 10 | no | 1.4–20 |
/// | narrow gauge 100 ± 10 | [90, 110] | 1.0–1.07 |
/// | counter readings near 1e6 | no | 1.5–22 |
/// | per-series levels ± 10%, levels over [1, 1e5] | no | 1.4–20 |
/// | per-series levels ± 10%, levels over [1, 1e5] | [1, 1e5] | 1.4–12 |
///
/// Few rows per state are stored sparse and priced closely. Past them the
/// price is the whole column range dense: values narrower than it (without
/// a declared range, anything narrower than nine decades) are
/// over-charged. The count width follows the rows: from 256 to 65,535 rows
/// a state whose largest count fits a byte is charged 2 bytes per count.
/// Values outside the assumed nine decades with no declared range can be
/// under-charged. Where this estimate is far from a deployment's states,
/// its measured table's bytes per state, fitted from its stored states per
/// family, replace it.
///
/// Example: at α = 0.01, a state of 10 values with no declared range is
/// stored sparse in at most 10 buckets at 1-byte counts: 88 + 4 + 40 + 16 =
/// 148 bytes. A gauge declared within [90, 110] spans 12 buckets: 600 rows,
/// dense at 2-byte counts, 88 + 4 + 24 = 116 bytes.
fn ddsketch_state_bytes(alpha: f64, input_rows: StateInputRows) -> u64 {
    // The view's header, and the state's 4-byte offset in its column.
    const HEADER_BYTES: u64 = 88;
    const OFFSET_BYTES: u64 = 4;
    let padded = |bytes: u64| bytes.div_ceil(8) * 8;
    let (_, stores) = ddsketch_span(alpha, input_rows);
    // The most buckets a store's values can occupy: the declared (or
    // assumed) range's, plus one for a range that does not start on a
    // bucket boundary.
    let column_span = match input_rows.rows_per_state() {
        Some(0 | 1) => 1,
        _ => {
            let ratio = input_rows
                .value_magnitude_range
                .unwrap_or(DDSKETCH_ASSUMED_MAGNITUDE_RANGE)
                .magnitude_ratio();
            ddsketch_buckets_spanned(alpha, ratio) + 1
        }
    };
    let Some(rows) = input_rows.rows_per_state() else {
        return HEADER_BYTES + OFFSET_BYTES + stores * padded(8 * column_span);
    };
    // No count exceeds the rows, so they bound the width.
    let count_bytes = match rows {
        0..=0xff => 1,
        0x100..=0xffff => 2,
        0x1_0000..=0xffff_ffff => 4,
        _ => 8,
    };
    let dense = stores * padded(count_bytes * column_span);
    // At most one occupied bucket per row, however the rows split between
    // the stores; each further store pads its two arrays.
    let occupied = rows.max(1).min(stores * column_span);
    let sparse = padded(4 * occupied) + padded(count_bytes * occupied) + (stores - 1) * 16;
    HEADER_BYTES + OFFSET_BYTES + dense.min(sparse)
}

/// The bytes a DDSketch still receiving values holds (the current window
/// of a chain, a query-time build, a merge target): its stores grow 128
/// slots at a time, so per store one first block (1 KiB) plus a counter
/// for each bucket beyond the first ([`ddsketch_span`]).
fn ddsketch_open_state_bytes(alpha: f64, input_rows: StateInputRows) -> u64 {
    use asap_sketchlib::DdSketch;
    let struct_bytes = std::mem::size_of::<DdSketch>() as u64;
    let block_bytes =
        DdSketch::estimated_memory_bytes(alpha, 1, 1.0, 1.0, false) as u64 - struct_bytes;
    let (span, stores) = ddsketch_span(alpha, input_rows);
    struct_bytes + stores * (block_bytes + 8 * (span - 1))
}

#[cfg(test)]
mod tests;
