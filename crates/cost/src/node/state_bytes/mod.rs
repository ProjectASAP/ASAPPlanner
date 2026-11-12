//! The bytes one summary or exact state holds and its update operations
//! per input row, from its parameters and the rows it folds.

use asap_types::ir::operator::AggIntent;
use asap_types::ir::schema::{DataType, FieldDataType, Schema};
use asap_types::summary_type::KLL_MAX_K;
use asap_types::summary_type::{SketchParams, EXACT_ACCUMULATOR_STATE_BYTES};

/// Plain values are 8 bytes, strings 16; summary columns are sized apart.
pub fn row_bytes(schema: &Schema) -> u64 {
    schema
        .fields
        .iter()
        .map(|f| match &f.dtype {
            FieldDataType::Plain(DataType::Utf8) => 16,
            _ => 8,
        })
        .sum::<u64>()
        .max(1)
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

/// Update operations per input row and bytes per state, from the sketch's
/// parameters and `input_rows` (the rows one state folds). The bytes are
/// `asap_sketchlib`'s `estimated_memory_bytes` for the type the executor
/// builds (Count-Min and Count Sketch over `f64` counters), given what the
/// state is assumed to hold:
/// - a Count-Min / Count Sketch top-k heap full of keys of
///   `ASSUMED_HEAP_KEY_BYTES`; each UnivMon layer's heap full of keys of
///   `ASSUMED_VALUE_KEY_BYTES`;
/// - DDSketch and KMV: the rows the state folds, at least one (any number
///   when unknown), each a distinct item for KMV; a DDSketch's values span
///   `DDSKETCH_ASSUMED_VALUE_RANGE` and fill its negative store too
///   unless proven non-negative.
///
/// KLL is priced without the library's estimate: its fixed bytes
/// (`kll_fixed_bytes`: the struct and its level array) plus the 8-byte
/// item array as `asap_sketchlib` grows it for the rows the state folds, at
/// most its full capacity (`kll_buffer_items`).
///
/// An exact accumulator's state is the executor's per-group state,
/// [`EXACT_ACCUMULATOR_STATE_BYTES`] for every kind.
///
/// Update operations per input row: one counter per row of depth for
/// Count-Min and Count-Sketch, plus one heap update for their top-k variants;
/// for UnivMon, the same per layer it updates (`univmon_expected_layers`);
/// 1 for every other sketch.
pub fn summary_shape(family: &FieldDataType, input_rows: StateInputRows) -> (f64, u64) {
    match family {
        FieldDataType::Sketch(..) => {
            let _ = input_rows;
            todo!("#1207 PR 6: the state shape of θ from the candidate's LogicalProperties::parameters")
        }
        FieldDataType::ExactAggregate(..) => (1.0, EXACT_ACCUMULATOR_STATE_BYTES),
        _ => (1.0, 8),
    }
}

/// What one summary state's bytes depend on besides its parameters (only
/// a KLL's, a DDSketch's and a KMV's do): the input rows it folds and the
/// sign of their values.
/// The rows are whatever its producer emits (raw data samples, rates, sums),
/// not a summary sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StateInputRows {
    /// Input rows one state folds, the average over its states rounded up;
    /// `None` when unknown, which sizes it for every value it can be assumed
    /// to see.
    pub rows_per_state: Option<u64>,
    /// Whether every value folded is proven non-negative (raw data samples of
    /// sources declared non-negative), so a DDSketch fills no negative store.
    pub non_negative: bool,
}

impl StateInputRows {
    /// Unknown rows of unknown sign: the most a state is assumed to hold.
    pub const UNKNOWN: Self = Self {
        rows_per_state: None,
        non_negative: false,
    };
}

/// [`summary_shape`] of one sketch: its update operations per row and its
/// bytes per state. With [`StateInputRows::UNKNOWN`], the bytes are also the
/// size the measured table interpolates between
/// (`crate::measured`).
pub fn sketch_shape(params: &SketchParams, input_rows: StateInputRows) -> (f64, u64) {
    use asap_sketchlib::{
        Classic, CountMinF64, CountMinSketchWithHeap, CountSketch, CountSketchWithHeap, DataInput,
        DdSketch, HyperLogLog, Kmv, RegularPath, UnivMon, Vector2D,
    };
    let heap_key = DataInput::Bytes(&[0; ASSUMED_HEAP_KEY_BYTES]);
    let rows = input_rows.rows_per_state.map_or(usize::MAX, |rows| {
        usize::try_from(rows.max(1)).unwrap_or(usize::MAX)
    });
    match params {
        SketchParams::Hydra(hydra) => sketch_shape(&hydra.per_subpopulation(), input_rows),
        SketchParams::Cms { width, depth } => (
            f64::from(*depth),
            CountMinF64::estimated_memory_bytes(*depth as usize, *width as usize) as u64,
        ),
        SketchParams::CountSketch { width, depth } => (
            f64::from(*depth),
            CountSketch::<Vector2D<f64>, RegularPath>::estimated_memory_bytes(
                *depth as usize,
                *width as usize,
            ) as u64,
        ),
        SketchParams::CmsWithHeap {
            width,
            depth,
            heap_size,
        } => (
            f64::from(*depth) + 1.0,
            CountMinSketchWithHeap::<Vector2D<f64>, RegularPath>::estimated_memory_bytes(
                *depth as usize,
                *width as usize,
                *heap_size as usize,
                &heap_key,
            ) as u64,
        ),
        SketchParams::CountSketchWithHeap {
            width,
            depth,
            heap_size,
        } => (
            f64::from(*depth) + 1.0,
            CountSketchWithHeap::<Vector2D<f64>, RegularPath>::estimated_memory_bytes(
                *depth as usize,
                *width as usize,
                *heap_size as usize,
                &heap_key,
            ) as u64,
        ),
        SketchParams::Kll { k, .. } => (
            1.0,
            kll_fixed_bytes() + 8 * kll_buffer_items(*k, input_rows),
        ),
        // The executor builds no DDSketch outside α in (0, 1), where
        // sketchlib's estimate panics: priced as an empty one.
        SketchParams::DDSketch { alpha } if !(*alpha > 0.0 && *alpha < 1.0) => {
            (1.0, std::mem::size_of::<DdSketch>() as u64)
        }
        SketchParams::DDSketch { alpha } => (
            1.0,
            DdSketch::estimated_memory_bytes(
                *alpha,
                rows,
                1.0,
                DDSKETCH_ASSUMED_VALUE_RANGE,
                !input_rows.non_negative,
            ) as u64,
        ),
        SketchParams::Hll { precision, .. } => (
            1.0,
            HyperLogLog::<Classic>::estimated_memory_bytes(usize::from(*precision)) as u64,
        ),
        SketchParams::Kmv { k, .. } => (1.0, Kmv::estimated_memory_bytes(*k as usize, rows) as u64),
        SketchParams::UnivMon {
            heap_size,
            sketch_rows,
            sketch_cols,
            layers,
            ..
        } => (
            univmon_expected_layers(*layers) * (f64::from(*sketch_rows) + 1.0),
            UnivMon::estimated_memory_bytes(
                *heap_size as usize,
                *sketch_rows as usize,
                *sketch_cols as usize,
                usize::from(*layers),
                &DataInput::Bytes(&[0; ASSUMED_VALUE_KEY_BYTES]),
            ) as u64,
        ),
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

/// Layers a UnivMon row updates in expectation. `asap_sketchlib` updates
/// layers `0..=b`, with bottom layer `b` the index of the first zero among
/// hash bits `1..layers`, capped at `layers - 1`; so it reaches layer `j`
/// with probability `2^-j`, and the expectation is
/// `sum_{j < layers} 2^-j = 2 - 2^(1 - layers)`.
fn univmon_expected_layers(layers: u8) -> f64 {
    2.0 - 2f64.powi(1 - i32::from(layers.max(1)))
}

/// Ratio of the largest to the smallest value magnitude a DDSketch is
/// assumed to see: nine decades, e.g. latencies from 1 ns to 1 s, priced as
/// magnitudes in `[1, 1e9]` (its bins depend on the ratio alone). An
/// assumption, not workload evidence: the data workload declares no value
/// range.
const DDSKETCH_ASSUMED_VALUE_RANGE: f64 = 1e9;

#[cfg(test)]
mod tests;
