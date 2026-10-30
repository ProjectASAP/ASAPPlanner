//! The bytes one summary or exact state holds and its update operations
//! per input row, from its parameters and the rows it folds.

use asap_types::ir::operator::AggIntent;
use asap_types::ir::schema::sketch::KLL_MAX_K;
use asap_types::ir::schema::{
    DataType, FieldDataType, Schema, SketchParams, EXACT_ACCUMULATOR_STATE_BYTES,
};

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
        FieldDataType::Sketch(kind, _) => sketch_shape(kind.params(), input_rows),
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
/// ([`crate::measured`]).
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
mod tests {
    use super::*;
    use asap_types::ir::schema::{GroupingStrategy, SketchAlgorithm, SketchKind};

    /// An exact accumulator's state is priced at the executor's per-group
    /// state, 56 bytes for every kind (a counter's state alone is 48), so a
    /// pane chain of exact sums keeps 56 bytes per pane and group.
    #[test]
    fn exact_state_is_priced_at_the_executors_state_size() {
        use asap_types::ir::schema::{ExactKind, ExactParams};
        for (kind, params) in [
            (ExactKind::Sum, ExactParams::Sum),
            (ExactKind::Increase, ExactParams::Increase),
            (ExactKind::Rate, ExactParams::Rate),
        ] {
            assert_eq!(
                summary_shape(
                    &FieldDataType::ExactAggregate(kind, params),
                    StateInputRows::UNKNOWN
                ),
                (1.0, 56)
            );
        }
        assert_eq!(asap_types::ir::schema::EXACT_ACCUMULATOR_STATE_BYTES, 56);
    }

    /// The bytes `asap_sketchlib`'s DDSketch holds after adding `values`.
    fn library_ddsketch_bytes(alpha: f64, values: &[f64]) -> u64 {
        let mut sketch = asap_sketchlib::sketches::DdSketch::new(alpha);
        for value in values {
            sketch.add(value);
        }
        sketch.memory_bytes() as u64
    }

    /// A DDSketch is priced for what `asap_sketchlib` can hold after its
    /// rows: at least what it holds for a few samples, for samples of either
    /// sign, and for many samples over nine decades; a store less for values
    /// declared non-negative. Many rows are priced as unknown ones.
    #[test]
    fn ddsketch_state_bytes_bound_the_library() {
        let alpha = 0.05;
        let priced = |rows_per_state, non_negative| {
            sketch_shape(
                &SketchParams::DDSketch { alpha },
                StateInputRows {
                    rows_per_state,
                    non_negative,
                },
            )
            .1
        };
        // Ten Pareto(2) samples (scale 1), one 1 s pane of a 10 Hz series.
        let pareto: Vec<f64> = (0..10)
            .map(|i| (1.0 - (f64::from(i) + 0.5) / 10.0).powf(-0.5))
            .collect();
        let signed: Vec<f64> = pareto.iter().map(|v| -v).chain(pareto.clone()).collect();
        let decades: Vec<f64> = (0..10_000)
            .map(|i| 10f64.powf(9.0 * f64::from((i * 7_919) % 10_000) / 10_000.0))
            .collect();
        let signed_decades: Vec<f64> = decades
            .iter()
            .enumerate()
            .map(|(i, v)| if i % 2 == 0 { -v } else { *v })
            .collect();
        for (values, non_negative) in [
            (&pareto[..], true),
            (&[-2.0][..], false),
            (&[2.0][..], false),
            (&[-2.0, 2.0][..], false),
            (&signed[..], false),
            (&decades[..], true),
            (&signed_decades[..], false),
        ] {
            let rows = Some(values.len() as u64);
            assert!(
                library_ddsketch_bytes(alpha, values) <= priced(rows, non_negative),
                "{} values, non-negative: {non_negative}",
                values.len()
            );
        }
        assert!(priced(Some(10_000), true) < priced(Some(10_000), false));
        assert_eq!(priced(Some(10_000), false), priced(None, false));
    }

    /// DDSketch state grows as its relative accuracy α tightens.
    #[test]
    fn ddsketch_state_grows_as_alpha_shrinks() {
        let bytes = |alpha| {
            summary_shape(
                &FieldDataType::Sketch(
                    SketchKind::new(SketchAlgorithm::DDSketch, SketchParams::DDSketch { alpha }),
                    GroupingStrategy::default(),
                ),
                StateInputRows::UNKNOWN,
            )
            .1
        };
        assert!(
            bytes(0.1) < bytes(0.01),
            "{} vs {}",
            bytes(0.1),
            bytes(0.01)
        );
        assert!(
            bytes(0.01) < bytes(0.001),
            "{} vs {}",
            bytes(0.01),
            bytes(0.001)
        );
    }

    /// Every sketch family's state is sized from its parameters, as
    /// `asap_sketchlib` allocates it: an HLL's registers, a KMV's `k`
    /// hashes once it has seen `k` distinct items, and a UnivMon's layers
    /// once keys of the assumed size have churned through every heap.
    #[test]
    fn summary_state_is_sized_from_sketch_params() {
        use asap_sketchlib::DataInput;
        let bytes = |algorithm, params| {
            summary_shape(
                &FieldDataType::Sketch(
                    SketchKind::new(algorithm, params),
                    GroupingStrategy::default(),
                ),
                StateInputRows::UNKNOWN,
            )
            .1
        };
        assert_eq!(
            bytes(
                SketchAlgorithm::Hll,
                SketchParams::Hll {
                    precision: 14,
                    relative_error: None
                }
            ),
            asap_sketchlib::HyperLogLog::<asap_sketchlib::Classic>::new().memory_bytes() as u64
        );
        let mut kmv: asap_sketchlib::Kmv = asap_sketchlib::Kmv::new(1_000);
        for i in 0..2_000u64 {
            kmv.update(&DataInput::U64(i));
        }
        assert_eq!(
            bytes(
                SketchAlgorithm::Kmv,
                SketchParams::Kmv {
                    k: 1_000,
                    relative_error: None
                }
            ),
            kmv.memory_bytes() as u64
        );
        let mut univmon = asap_sketchlib::UnivMon::new(8, 3, 64, 2);
        for i in 0..10_000u64 {
            let mut key = [0; ASSUMED_VALUE_KEY_BYTES];
            key[..8].copy_from_slice(&i.to_le_bytes());
            univmon.update_weighted(&DataInput::Bytes(&key), 1);
        }
        assert_eq!(
            bytes(
                SketchAlgorithm::UnivMon,
                SketchParams::UnivMon {
                    heap_size: 8,
                    sketch_rows: 3,
                    sketch_cols: 64,
                    layers: 2,
                    distinct_failure_probability: None,
                }
            ),
            univmon.memory_bytes() as u64
        );
    }

    /// KLL state whose samples are unknown is charged its fixed 744 bytes
    /// and the item array at the capacity `asap_sketchlib` grows it to:
    /// `compute_max_capacity(k, 8)` 8-byte items, with `k` capped at 26 602.
    #[test]
    fn kll_state_is_sized_as_asap_sketchlib_allocates_it() {
        let bytes = |k| {
            summary_shape(
                &FieldDataType::Sketch(
                    SketchKind::new(
                        SketchAlgorithm::Kll,
                        SketchParams::Kll {
                            k,
                            failure_probability: None,
                        },
                    ),
                    GroupingStrategy::default(),
                ),
                StateInputRows::UNKNOWN,
            )
            .1
        };
        assert_eq!(bytes(269), 744 + 8 * 1_206);
        assert_eq!(bytes(26_602), 744 + 8 * 80_118);
        assert_eq!(bytes(30_000), 744 + 8 * 80_118);
    }

    /// A KLL state is charged what `asap_sketchlib` allocates for the
    /// samples it sees, not its full capacity: its fixed struct and level
    /// array, plus the item array the library starts empty and doubles from
    /// 16 items as samples arrive, up to `compute_max_capacity(k, 8)`. E.g.
    /// a 10 s pane of a series sampled every 15 s sees one sample: 744 + 128
    /// = 872 bytes, where the full capacity at `k` = 269 is 1,206 items.
    #[test]
    fn kll_state_bytes_grow_with_the_samples_it_sees() {
        let k = 269;
        // The library's bytes after `n` samples.
        let library_bytes = |n: u32| {
            let mut sketch = asap_sketchlib::Kll::<f64>::with_k(k as i32);
            for i in 0..n {
                sketch.update(&f64::from(i));
            }
            sketch.memory_bytes() as u64
        };
        let model_bytes = |rows_per_state| {
            sketch_shape(
                &SketchParams::Kll {
                    k,
                    failure_probability: None,
                },
                StateInputRows {
                    rows_per_state,
                    non_negative: true,
                },
            )
            .1
        };
        // An empty sketch holds its fixed bytes; one sample adds 16 items.
        assert_eq!(library_bytes(0), kll_fixed_bytes());
        assert_eq!(library_bytes(0), 744);
        assert_eq!(library_bytes(1), 872);
        // Up to `k` samples nothing compacts, so the library holds exactly
        // the fixed bytes and the grown array (and no merge scratch space).
        for n in [1, 4, 16, 17, 100, k] {
            assert_eq!(
                model_bytes(Some(u64::from(n))),
                library_bytes(n),
                "{n} samples"
            );
        }
        // Many samples are charged the full capacity, the most the array
        // grows to.
        assert_eq!(model_bytes(Some(5_000)), 744 + 8 * 1_206);
        // A state assumed to see no sample holds at least one, as a
        // DDSketch's does; unknown samples fill the full capacity.
        assert_eq!(model_bytes(Some(0)), 872);
        assert_eq!(model_bytes(None), 744 + 8 * 1_206);
    }

    /// Plan selection prices each state the executor builds per group at
    /// what the executor charges for it (#947). Each family is built
    /// through the executor's factory and fed `n` rows shaped as the
    /// pricing assumes: signed values over [`DDSKETCH_ASSUMED_VALUE_RANGE`],
    /// label values of [`ASSUMED_VALUE_KEY_BYTES`] keys for UnivMon, or
    /// keyed rows whose tuple keys are [`ASSUMED_HEAP_KEY_BYTES`] long. The
    /// heap families' shapes are dominated by their heaps. The charge is
    /// the sketch's bytes plus the accumulator's own fields, named per
    /// family. It equals the priced bytes where `asap_sketchlib`'s estimate
    /// is exact (Count-Min, Count Sketch, HLL, KMV at `n` distinct items) and
    /// for KLL up to `k` rows. For DDSketch, the heap sketches and UnivMon
    /// the price bounds it, by at most 1.5 times once the state is full;
    /// UnivMon's price also bounds the bytes its sketch holds.
    /// KLL past `k` rows is checked only loosely: the planner's formula
    /// leaves out the compaction scratch space, a known gap until a
    /// sketchlib estimate without the CDF cache can price KLL.
    ///
    /// HydraCms is left out: plan selection prices an independent Count-Min
    /// per group, the executor one shared grid, so the two agree only at
    /// about 79 groups for a 5 × 32 grid of 64 × 3 cells (a follow-up).
    #[test]
    fn state_bytes_bound_the_executors_charge() {
        use asap_executor::factory::create_planner_accumulator;
        use asap_executor::summary_kernels::univmon::UnivMonAccumulator;
        use asap_executor::summary_kernels::weighted_frequency::WeightedFrequency;
        use asap_executor::summary_kernels::HllSketchAccumulator;
        use asap_executor::values::Value;
        use asap_types::ir::scalar::expr_ir::ColumnRef;
        use asap_types::ir::schema::{
            NonNegativeWeightProof, SummaryInputExpr, SummaryUpdate, WeightDomain,
        };
        use std::mem::{align_of, size_of};
        use SketchAlgorithm as A;
        /// How the priced bytes relate to the sketch's.
        enum Priced {
            Exact,
            /// Exact up to `k` rows; past them at most the formula's
            /// missing compaction scratch space over it.
            Kll {
                k: u32,
            },
            /// At least, and at most 1.5 times once the state is full.
            Bound,
        }
        /// The rows a family is fed.
        enum Rows {
            /// Signed values over the assumed range.
            Values,
            /// One label value a row, as Pass 1 feeds UnivMon a column.
            Labels,
            /// A label set and a weight a row.
            Keyed,
        }
        // (family, parameters, rows fed, how it is priced, the
        // accumulator's own fields, rows at which the state is full: four
        // times the heap for the heap sketches, so each heap churns).
        //
        // The accumulators' own fields, beside the sketch each wraps:
        // Count-Min / Count Sketch's width, depth and capacity, padded to
        // the struct's alignment; HLL's precision tag; UnivMon's two key
        // bounds.
        let weighted_frequency_shape =
            (3 * size_of::<usize>()).next_multiple_of(align_of::<WeightedFrequency>());
        let hll_precision_tag = size_of::<HllSketchAccumulator>()
            - size_of::<asap_sketchlib::HyperLogLog<asap_sketchlib::Classic>>();
        let univmon_key_bounds = 2 * size_of::<usize>();
        let families = [
            (
                A::Kll,
                SketchParams::Kll {
                    k: 200,
                    failure_probability: None,
                },
                Rows::Values,
                Priced::Kll { k: 200 },
                0,
                10_000,
            ),
            (
                A::DDSketch,
                SketchParams::DDSketch { alpha: 0.01 },
                Rows::Values,
                Priced::Bound,
                0,
                10_000,
            ),
            (
                A::Cms,
                SketchParams::Cms {
                    width: 1_024,
                    depth: 5,
                },
                Rows::Keyed,
                Priced::Exact,
                0,
                10_000,
            ),
            (
                A::CountSketch,
                SketchParams::CountSketch {
                    width: 1_024,
                    depth: 5,
                },
                Rows::Keyed,
                Priced::Exact,
                weighted_frequency_shape,
                10_000,
            ),
            (
                A::CmsWithHeap,
                SketchParams::CmsWithHeap {
                    width: 16,
                    depth: 1,
                    heap_size: 256,
                },
                Rows::Keyed,
                Priced::Bound,
                weighted_frequency_shape,
                1_024,
            ),
            (
                A::CountSketchWithHeap,
                SketchParams::CountSketchWithHeap {
                    width: 16,
                    depth: 1,
                    heap_size: 256,
                },
                Rows::Keyed,
                Priced::Bound,
                weighted_frequency_shape,
                1_024,
            ),
            (
                A::Hll,
                SketchParams::Hll {
                    precision: 12,
                    relative_error: None,
                },
                Rows::Values,
                Priced::Exact,
                hll_precision_tag,
                10_000,
            ),
            (
                A::Kmv,
                SketchParams::Kmv {
                    k: 1_024,
                    relative_error: None,
                },
                Rows::Values,
                Priced::Exact,
                0,
                10_000,
            ),
            // Its top layer sees one row in eight: 4 096 labels fill and
            // churn every layer's heap.
            (
                A::UnivMon,
                SketchParams::UnivMon {
                    heap_size: 256,
                    sketch_rows: 1,
                    sketch_cols: 16,
                    layers: 4,
                    distinct_failure_probability: None,
                },
                Rows::Labels,
                Priced::Bound,
                univmon_key_bounds,
                4_096,
            ),
        ];
        let value = SummaryUpdate::column(ColumnRef::SampleValue);
        let item = Some(SummaryInputExpr::Column(ColumnRef::Named("item".into())));
        let keyed = SummaryUpdate {
            item: item.clone(),
            ..value.clone()
        };
        let labeled = SummaryUpdate {
            item,
            weight: SummaryInputExpr::Constant(1.0),
            weight_domain: WeightDomain::NonNegative {
                proof: NonNegativeWeightProof::UnitCount,
            },
        };
        // A 16-byte label value: its key has the assumed size.
        let label = |i: u32| format!("{i:016}");
        assert_eq!(
            Value::Utf8(label(0).into()).key().unwrap().len(),
            ASSUMED_VALUE_KEY_BYTES
        );
        // Three 16-byte labels: a tuple key of the assumed size.
        let labels = |i: u32| {
            [
                label(i),
                "service-frontend".to_string(),
                "region-us-east-1".to_string(),
            ]
        };
        let key = asap_sketchlib::TupleKey::new(
            labels(0)
                .into_iter()
                .map(asap_sketchlib::KeyColumn::String)
                .collect(),
        );
        assert_eq!(key.to_bytes().unwrap().len(), ASSUMED_HEAP_KEY_BYTES);
        for (algorithm, params, rows, priced_as, fields, full) in &families {
            for n in [1, 100, *full] {
                let family = FieldDataType::Sketch(
                    SketchKind::new(*algorithm, params.clone()),
                    GroupingStrategy::default(),
                );
                let input = match rows {
                    Rows::Values => &value,
                    Rows::Labels => &labeled,
                    Rows::Keyed => &keyed,
                };
                let mut updater =
                    create_planner_accumulator(&family, input, &GroupingStrategy::default())
                        .unwrap();
                for i in 0..n {
                    match rows {
                        // Distinct magnitudes over the assumed range, in a
                        // scrambled order, alternating in sign.
                        Rows::Values => {
                            let at = f64::from(i * 7_919 % n) / f64::from(n);
                            let magnitude = DDSKETCH_ASSUMED_VALUE_RANGE.powf(at);
                            let sign = if i % 2 == 0 { 1.0 } else { -1.0 };
                            updater.update_single(sign * magnitude, 0).unwrap();
                        }
                        Rows::Labels => updater
                            .update_value(&Value::Utf8(label(i).into()), 0)
                            .unwrap(),
                        Rows::Keyed => updater.update_keyed(&labels(i), 1.0, 0).unwrap(),
                    }
                }
                let state = updater.into_accumulator();
                let charged = state.approx_memory_bytes();
                let priced = fields
                    + sketch_shape(
                        params,
                        StateInputRows {
                            rows_per_state: Some(u64::from(n)),
                            non_negative: false,
                        },
                    )
                    .1 as usize;
                let context = format!("{algorithm:?} after {n} rows: {priced} priced");
                match priced_as {
                    Priced::Exact => assert_eq!(charged, priced, "{context}"),
                    Priced::Kll { k } if n <= *k => assert_eq!(charged, priced, "{context}"),
                    // sketchlib bounds KLL's compaction scratch space at
                    // 2 (k - 1) items.
                    Priced::Kll { k } => assert!(
                        charged <= priced + 8 * 2 * (*k as usize - 1),
                        "{context}, {charged} charged"
                    ),
                    Priced::Bound => {
                        assert!(charged <= priced, "{context}, {charged} charged");
                        if n == *full {
                            assert!(
                                priced as f64 <= 1.5 * charged as f64,
                                "{context}, {charged} charged"
                            );
                        }
                    }
                }
                // UnivMon charges sketchlib's estimate itself, since its
                // `memory_bytes` walks the heaps: the sketch holds no more.
                // The estimate is a heap whose digest index has grown once
                // more under churn, which the test's labels do not reach: a
                // full heap holds about two thirds of it (1.52 times less
                // here), so this bound is 1.6 times rather than 1.5.
                if let Some(univmon) = state.as_any().downcast_ref::<UnivMonAccumulator>() {
                    let held = fields + univmon.sketch().memory_bytes();
                    assert!(held <= charged, "{context}, {held} held");
                    if n == *full {
                        assert!(priced as f64 <= 1.6 * held as f64, "{context}, {held} held");
                    }
                }
            }
        }
    }

    /// A UnivMon row updates layers `0..=b`, where its bottom layer `b` is
    /// geometric (`asap_sketchlib`'s `bottom_layer_for_hash`), so
    /// `2 - 2^(1 - layers)` layers in expectation; each updates one counter
    /// per sketch row and its heap.
    #[test]
    fn univmon_update_cost_is_expected_layers_times_rows_and_heap() {
        let ops = |layers| {
            summary_shape(
                &FieldDataType::Sketch(
                    SketchKind::new(
                        SketchAlgorithm::UnivMon,
                        SketchParams::UnivMon {
                            heap_size: 256,
                            sketch_rows: 5,
                            sketch_cols: 1_024,
                            layers,
                            distinct_failure_probability: None,
                        },
                    ),
                    GroupingStrategy::default(),
                ),
                StateInputRows::UNKNOWN,
            )
            .0
        };
        assert_eq!(ops(1), 6.0);
        assert_eq!(ops(2), 1.5 * 6.0);
        assert_eq!(ops(16), (2.0 - 2f64.powi(-15)) * 6.0);
    }
}
