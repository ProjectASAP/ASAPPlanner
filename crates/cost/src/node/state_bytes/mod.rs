//! The bytes one summary or exact state holds and its update operations
//! per input row, from its parameters and the rows it folds.

use crate::node::analytical::AnalyticalCostError;
use crate::node::operator_statistics::EdgeStatistics;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema, PROMQL_SERIES_IDENTITY};
use asap_types::summary_type::KLL_MAX_K;
use asap_types::summary_type::{SketchParams, EXACT_ACCUMULATOR_STATE_BYTES};
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

/// Bytes a summary state's output row holds beside its state: its labels
/// and its window's time, each at [`column_bytes`] except the series
/// identity, which a state's row holds its own copy of, at
/// `SERIES_IDENTITY_BYTES`. Example: a stored per-series Sum carrying
/// `job` holds its window time, `job` and the series identity: 8 + 16 + 64
/// = 88 bytes beside its 56-byte state, 144 in all, where the executor
/// holds 151 per state.
pub(crate) fn state_row_bytes_beside_state(schema: &Schema) -> u64 {
    schema
        .fields
        .iter()
        .map(|field| match &field.dtype {
            FieldDataType::Sketch(..) | FieldDataType::ExactAggregate(..) => 0,
            FieldDataType::Plain(DataType::Utf8) if field.name == PROMQL_SERIES_IDENTITY => {
                SERIES_IDENTITY_BYTES
            }
            _ => column_bytes(field),
        })
        .sum()
}

/// Bytes of the state one row of `edge` holds: its bytes per row without
/// what the row holds beside its state ([`state_row_bytes_beside_state`] of
/// the producer's `schema`). Example: 1,000 rows of stored per-series Sums
/// carrying `job`, 144,000 bytes, hold 144 − 88 = 56 bytes of state each.
pub(crate) fn state_bytes_per_row(edge: EdgeStatistics, schema: &Schema) -> u64 {
    edge.bytes_per_row()
        .saturating_sub(state_row_bytes_beside_state(schema))
}

/// Bytes of one series identity in a state's row: its serialized label
/// set, e.g. `{"instance":"host-17:9100","job":"api","region":"r1"}`
/// (about 55 bytes), and the column's 4-byte offset, rounded up. An
/// assumption: the data workload declares no label lengths.
const SERIES_IDENTITY_BYTES: u64 = 64;

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
/// - DDSketch: what it is expected to hold, not the most it can
///   ([`ddsketch_state_bytes`]).
///
/// KLL is priced without the library's estimate: its fixed bytes
/// (`kll_fixed_bytes`: the struct and its level array) plus the 8-byte
/// item array as `asap_sketchlib` grows it for the rows the state folds, at
/// most its full capacity (`kll_buffer_items`).
///
/// An exact accumulator's state is the executor's per-group state,
/// [`EXACT_ACCUMULATOR_STATE_BYTES`] for every kind.
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
        (FieldDataType::ExactAggregate(..), _) => EXACT_ACCUMULATOR_STATE_BYTES,
        _ => 8,
    })
}

/// What one summary state's bytes depend on besides its parameters (only
/// a KLL's, a DDSketch's and a KMV's do): the input rows it folds, the
/// sign of their values and, for a DDSketch, their magnitudes; and the
/// distinct values of the column it folds, which a KMV build's and merge's
/// CPU depend on.
/// The rows are whatever its producer emits (raw data samples, rates, sums),
/// not a summary sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StateInputRows {
    /// Input rows one state folds, the average over its states rounded up;
    /// `None` when unknown, which sizes it for every value it can be assumed
    /// to see.
    pub rows_per_state: Option<u64>,
    /// Whether every value folded is proven non-negative (a source column
    /// passed on unchanged, which every source declares non-negative), so a
    /// DDSketch fills no negative store.
    pub non_negative: bool,
    /// Bounds on `|value|` of the non-zero values folded into the state;
    /// `None` when not declared, which costs a DDSketch over the assumed
    /// `DDSKETCH_ASSUMED_MAGNITUDE_RANGE`. E.g. `Some(ValueMagnitudeRange {
    /// min_abs: 0.001, max_abs: 10000.0 })` for a latency in seconds.
    pub value_magnitude_range: Option<ValueMagnitudeRange>,
    /// The distinct values the folded column takes in one state, each
    /// folded row a draw from them: the fewer of the column's declared
    /// distinct values and the sources' bound per group; `None` when
    /// neither is declared, which takes every row's value as new. E.g.
    /// `Some(1_000_000)` for `user_id` over 1,000,000 users: KMV states of
    /// 12,000 rows each then fold about 11,929 distinct users, and states
    /// merged together share some of their retained hashes.
    pub distinct_values: Option<u64>,
}

impl StateInputRows {
    /// What one state folds when each group merges, from every input, that
    /// input's states (`Some((rows each folds, states per group))`): their
    /// rows summed, non-negative when all are, within the magnitude range
    /// spanning all of theirs, of the most distinct values any declares.
    /// `None` when any input's is unknown. Example:
    /// 360 states of 10 rows and 60 of 100 merge into one of 9,600 rows.
    pub(crate) fn merged(inputs: impl IntoIterator<Item = Option<(Self, u64)>>) -> Option<Self> {
        let mut merged: Option<Self> = None;
        for (rows, states) in inputs.into_iter().collect::<Option<Vec<_>>>()? {
            let input = Self {
                rows_per_state: rows
                    .rows_per_state
                    .map(|per_state| per_state.saturating_mul(states)),
                ..rows
            };
            merged = Some(match merged {
                None => input,
                Some(sum) => Self {
                    rows_per_state: sum
                        .rows_per_state
                        .zip(input.rows_per_state)
                        .map(|(a, b)| a.saturating_add(b)),
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
        rows_per_state: None,
        non_negative: false,
        value_magnitude_range: None,
        distinct_values: None,
    };
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
    let rows = input_rows.rows_per_state.map_or(usize::MAX, |rows| {
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
        SketchParams::Kll { k, .. } => kll_fixed_bytes() + 8 * kll_buffer_items(*k, input_rows),
        // The executor builds no DDSketch outside α in (0, 1), where
        // sketchlib's estimate panics: priced as an empty one.
        SketchParams::DDSketch { alpha } if !(*alpha > 0.0 && *alpha < 1.0) => {
            std::mem::size_of::<DdSketch>() as u64
        }
        // Over the declared magnitude range, or the one assumed when none
        // is declared (`assumed_magnitude_range`).
        SketchParams::DDSketch { alpha } => ddsketch_state_bytes(
            *alpha,
            input_rows,
            input_rows
                .value_magnitude_range
                .or_else(|| assumed_magnitude_range(params, input_rows)),
        ),
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

/// Levels `asap_sketchlib`'s KLL has room for (`MAX_LEVELS`).
const KLL_MAX_LEVELS: usize = 61;

/// Bytes every `asap_sketchlib` KLL state holds whatever its samples: the
/// struct and the 8-byte level-boundary array of `KLL_MAX_LEVELS + 1` entries it
/// allocates on creation (`Kll::init_internal`). 744 bytes, an empty
/// sketch's `memory_bytes()`.
fn kll_fixed_bytes() -> u64 {
    (std::mem::size_of::<asap_sketchlib::Kll<f64>>() + 8 * (KLL_MAX_LEVELS + 1)) as u64
}

/// The most items `asap_sketchlib`'s KLL holds for `k`, the capacity its item
/// array grows to (`Kll::init_internal` → `compute_max_capacity(k, 8)`): over its
/// [`KLL_MAX_LEVELS`] levels,
/// `max(ceil(k · (2/3)^i), 8)` each, with `k` capped at [`KLL_MAX_K`] as the
/// library caps it. 1,206 items at `k` = 269.
fn kll_capacity_items(k: u32) -> u64 {
    const MIN_LEVEL_CAPACITY: f64 = 8.0;
    let k = f64::from(k.min(KLL_MAX_K));
    let mut scale = 1.0_f64;
    let mut total = 0;
    for _ in 0..KLL_MAX_LEVELS {
        total += (k * scale).ceil().max(MIN_LEVEL_CAPACITY) as u64;
        scale *= 2.0 / 3.0;
    }
    total
}

/// Items `asap_sketchlib`'s KLL for `k` allocates over `input_rows`: its
/// item array starts empty and grows when full, to at least twice its
/// length and at least 16 items (`INITIAL_BUFFER_ITEMS`), at most its full
/// capacity ([`kll_capacity_items`]). So `n` rows take the smallest
/// `16 · 2^j` ≥ `n`, capped at the capacity: exactly what the library holds
/// up to `max(k, 8)` rows, before anything compacts; 16 items for one row,
/// the capacity when the rows are unknown. Past that, compaction keeps the
/// array within this size, but the library also holds merge scratch space,
/// not modelled here, that adds up to about a fifth. A state is assumed to
/// fold at least one row, as a DDSketch's is.
fn kll_buffer_items(k: u32, input_rows: StateInputRows) -> u64 {
    const INITIAL_BUFFER_ITEMS: u64 = 16;
    let capacity = kll_capacity_items(k);
    match input_rows.rows_per_state {
        None => capacity,
        Some(rows_per_state) => {
            let grown = rows_per_state
                .max(1)
                .div_ceil(INITIAL_BUFFER_ITEMS)
                .next_power_of_two();
            grown.saturating_mul(INITIAL_BUFFER_ITEMS).min(capacity)
        }
    }
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
/// state is assumed to span when the workload declares nothing about its
/// values: `rows` samples of an exponential distribution, as latencies
/// roughly are, whose largest is about `H_n` times their mean and whose
/// smallest has a log about `ln(mean / n) − γ`, so a ratio of about
/// `n · H_n · e^γ`. Example: 10 values span a ratio of 52, 3,000 values
/// one of 4.6e4.
///
/// The default when the data workload declares nothing about the values,
/// not workload evidence. At α = 0.01, values with a floor (lognormal
/// latencies, σ = 1) span less and are over-charged 1.1x; a narrow gauge
/// (100 ± 10), whose states hold one first block of buckets, 2.4x at 10
/// values per state to 5.6x at 36,000; values with a heavier tail than an
/// exponential (Pareto) are under-charged.
fn exponential_values_magnitude_ratio(rows: u64) -> f64 {
    let n = rows.max(1) as f64;
    let harmonic = n.ln() + std::f64::consts::EULER_GAMMA + 1.0 / (2.0 * n);
    n * harmonic * std::f64::consts::EULER_GAMMA.exp()
}

/// The bytes a DDSketch of relative accuracy `alpha` is expected to hold
/// after folding `input_rows`: its struct, and per sign it holds (one when
/// its values are proven non-negative or it folds one row, else two) the
/// dense bucket store `asap_sketchlib` grows, one first block of buckets
/// (`DdSketch::estimated_memory_bytes` of one value) plus one 8-byte
/// counter per bucket its values span beyond the first.
///
/// The span is that of the values the state folds, not of the column: the
/// smaller of the magnitude ratio its rows are assumed to span
/// ([`exponential_values_magnitude_ratio`]) and the ratio of
/// `column_range`, its column's declared `column_magnitudes`, or the range
/// [`assumed_magnitude_range`] assumes when none is declared; without known
/// rows, the ratio of `column_range`. The library's worst case over every order of
/// inserts and merges (`estimated_memory_bytes`) is about twice the span
/// over the whole range, which is not what a state holds on average.
///
/// Example: at α = 0.01, 10 exponential samples span a ratio of 52, 198
/// buckets: 144 + 1,024 + 8 × 197 = 2,744 bytes, where `asap_sketchlib`
/// holds 2,910 on average; 3,000 samples span 538 buckets, 5,464 bytes
/// against 5,769 held. The library's bound for either is 18,752. A gauge
/// declared within [90, 110] spans a ratio of 1.22, 11 buckets: 1,248
/// bytes for any number of rows.
fn ddsketch_state_bytes(
    alpha: f64,
    input_rows: StateInputRows,
    column_range: Option<ValueMagnitudeRange>,
) -> u64 {
    use asap_sketchlib::DdSketch;
    let struct_bytes = std::mem::size_of::<DdSketch>() as u64;
    // One value allocates one store's first block.
    let block_bytes =
        DdSketch::estimated_memory_bytes(alpha, 1, 1.0, 1.0, false) as u64 - struct_bytes;
    let sketch = DdSketch::new(alpha);
    // Buckets a ratio of `ratio` between the largest and smallest
    // magnitude spans, at least one.
    let buckets_spanned = |ratio: f64| -> u64 {
        let ratio = ratio.max(1.0);
        match (sketch.bucket_index_for(ratio), sketch.bucket_index_for(1.0)) {
            (Some(high), Some(low)) => u64::try_from(i64::from(high) - i64::from(low) + 1)
                .unwrap_or(1)
                .max(1),
            _ => 1,
        }
    };
    let stores = match (input_rows.non_negative, input_rows.rows_per_state) {
        (true, _) | (false, Some(0 | 1)) => 1,
        (false, _) => 2,
    };
    let rows_per_store = input_rows
        .rows_per_state
        .map(|rows| rows.div_ceil(stores).max(1));
    // The most the column's values span: its declared range, else the
    // assumed one.
    let column_ratio = column_range.map_or(f64::INFINITY, |range| range.magnitude_ratio());
    let span = buckets_spanned(rows_per_store.map_or(column_ratio, |rows| {
        exponential_values_magnitude_ratio(rows).min(column_ratio)
    }));
    let one_value = rows_per_store == Some(1);
    let buckets_beyond_first = if one_value { 0 } else { span - 1 };
    struct_bytes + stores * (block_bytes + 8 * buckets_beyond_first)
}

#[cfg(test)]
mod tests;
