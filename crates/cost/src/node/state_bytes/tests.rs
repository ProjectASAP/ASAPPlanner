use super::*;
use asap_types::summary_type::SketchAlgorithm;

/// An exact accumulator's state is priced at the executor's sealed
/// record of its kind: a sum 24 bytes, a counter 49 (its 48 and a
/// validity bit), so a tumbling-window chain of exact sums keeps
/// 24 bytes per tumbling window and group.
#[test]
fn exact_state_is_priced_at_the_executors_sealed_record() {
    use asap_types::summary_type::{ExactKind, ExactParams};
    for (kind, params, bytes) in [
        (ExactKind::Sum, ExactParams::Sum, 24),
        (ExactKind::Max, ExactParams::Max, 9),
        (ExactKind::Counter, ExactParams::Counter, 49),
    ] {
        assert_eq!(
            summary_state_bytes(
                &FieldDataType::ExactAggregate(kind, params),
                None,
                StateInputRows::UNKNOWN
            ),
            Ok(bytes)
        );
    }
}

/// Merging states keeps the larger column's distinct values, since they
/// describe the column the states were built from, not one state: states
/// over 1,000 and 1,000,000 users merge as 1,000,000; with either unknown,
/// unknown. Rows add up per state.
#[test]
fn merged_input_rows_keep_the_larger_distinct_values() {
    let rows = |rows_per_state, distinct_values| StateInputRows {
        folded_rows_per_state: Some((rows_per_state) as f64),
        non_negative: true,
        value_magnitude_range: None,
        distinct_values,
    };
    let merged = StateInputRows::merged([
        Some((rows(12_000, Some(1_000)), 2.0)),
        Some((rows(6_000, Some(1_000_000)), 1.0)),
    ])
    .unwrap();
    assert_eq!(merged.rows_per_state(), Some(30_000));
    assert_eq!(merged.distinct_values, Some(1_000_000));
    let merged = StateInputRows::merged([
        Some((rows(12_000, Some(1_000)), 1.0)),
        Some((rows(6_000, None), 1.0)),
    ])
    .unwrap();
    assert_eq!(merged.distinct_values, None);
}

/// The rows a family is fed.
#[derive(Debug)]
enum Rows {
    /// One sample value a row.
    Values,
    /// One latency-like sample value a row ([`latency_like`]).
    LatencyLike,
    /// One label value a row, as Pass 1 feeds UnivMon a column.
    Labels,
    /// A label set and a weight a row.
    Keyed,
}

impl Rows {
    /// How many rows [`priced_bytes_agree_with_the_executor`] feeds a
    /// family of these rows, priced for the same count. 10,000 values: a
    /// KLL state's charge reaches its priced size only past about 3,000
    /// rows. 1,000 label rows: the label families' grids have a fixed
    /// size, so their ratio is the same at any count, and 1,000 rows over
    /// 1,000 keys still churn a heap of 10; at 10,000 rows Count Sketch
    /// with a heap alone took about 22 s in a debug build.
    fn fed(&self) -> u32 {
        match self {
            Rows::Values | Rows::LatencyLike => 10_000,
            Rows::Labels | Rows::Keyed => 1_000,
        }
    }

    /// The update a family fed these rows reads: the sample value; an
    /// `item` label counted once a row; or an `item` label set weighted by
    /// the sample value.
    fn update(&self) -> asap_types::summary_type::SummaryUpdate {
        use asap_types::ir::scalar::expr_ir::ColumnRef;
        use asap_types::summary_type::{SummaryInputExpr, SummaryUpdate};
        let item = Some(SummaryInputExpr::Column(ColumnRef::Named("item".into())));
        match self {
            Rows::Values | Rows::LatencyLike => SummaryUpdate::column(ColumnRef::SampleValue),
            Rows::Labels => SummaryUpdate {
                item,
                weight: SummaryInputExpr::Constant(1.0),
            },
            Rows::Keyed => SummaryUpdate {
                item,
                ..SummaryUpdate::column(ColumnRef::SampleValue)
            },
        }
    }
}

/// A 16-byte label value, `i` zero-padded: its key has
/// [`ASSUMED_VALUE_KEY_BYTES`].
fn label(i: u32) -> String {
    format!("{i:016}")
}

/// Three 16-byte labels, the first [`label`]`(i)`: a tuple key of
/// [`ASSUMED_HEAP_KEY_BYTES`].
fn labels(i: u32) -> [String; 3] {
    [
        label(i),
        "service-frontend".to_string(),
        "region-us-east-1".to_string(),
    ]
}

/// The bytes the executor holds per sealed DDSketch state of relative
/// accuracy `alpha`, one state per entry of `series`: a column of them
/// sealed (in the view layout), less the column's one offset beyond one
/// per state, over the states.
fn sealed_ddsketch_bytes(alpha: f64, series: &[Vec<f64>]) -> f64 {
    let family = FieldDataType::Sketch(SketchAlgorithm::DDSketch);
    let mut column =
        asap_executor::StateColumnUpdater::new(&family, Some(&SketchParams::DDSketch { alpha }))
            .unwrap();
    column.ensure_groups(series.len()).unwrap();
    for (group, values) in series.iter().enumerate() {
        for &value in values {
            column.update(group as u32, value, 0).unwrap();
        }
    }
    (column.seal().state_memory_bytes() - 4) as f64 / series.len() as f64
}

/// A uniform draw in [0, 1) from a fixed hash of `x`.
fn uniform(mut x: u64) -> f64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^= x >> 33;
    (x >> 11) as f64 / (1_u64 << 53) as f64
}

/// `n` latency-like values of series `series`: draws of an exponential
/// distribution of mean 10, from a fixed hash of `(series, i)`.
fn latency_like(series: u64, n: u32) -> Vec<f64> {
    (0..u64::from(n))
        .map(|i| -(1.0 - uniform((series << 32) ^ i)).ln() * 10.0)
        .collect()
}

/// `n` values of series `series` with a floor, as measured latencies have:
/// draws of a lognormal distribution of median 10 ms and σ = 1 (half of
/// them within 5–20 ms), from a fixed hash of `(series, i)`.
fn lognormal(series: u64, n: u32) -> Vec<f64> {
    (0..u64::from(n))
        .map(|i| {
            // Box–Muller over two hashed uniforms.
            let u1 = 1.0 - uniform((series << 32) ^ (2 * i));
            let u2 = uniform((series << 32) ^ (2 * i + 1));
            let normal = (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos();
            10.0 * normal.exp()
        })
        .collect()
}

/// `n` values of series `series` of a narrow gauge: uniform in 100 ± 10.
fn narrow_gauge(series: u64, n: u32) -> Vec<f64> {
    (0..u64::from(n))
        .map(|i| 90.0 + 20.0 * uniform((series << 32) ^ i))
        .collect()
}

/// `n` values of series `series` spread evenly in log over seven decades,
/// [0.001, 10000], in random order, from a fixed hash of `(series, i)`.
fn log_uniform_over_seven_decades(series: u64, n: u32) -> Vec<f64> {
    (0..u64::from(n))
        .map(|i| 0.001 * 10f64.powf(7.0 * uniform((series << 32) ^ i)))
        .collect()
}

/// The mean bytes `asap_sketchlib`'s DDSketch holds over 20 series of `n`
/// values each from `values`, of both signs when `signed`.
fn library_ddsketch_mean_bytes(
    alpha: f64,
    n: u32,
    signed: bool,
    values: fn(u64, u32) -> Vec<f64>,
) -> f64 {
    let series: Vec<Vec<f64>> = (0..20)
        .map(|s| {
            values(s, n)
                .into_iter()
                .enumerate()
                .map(|(i, v)| if signed && i % 2 == 0 { -v } else { v })
                .collect()
        })
        .collect();
    sealed_ddsketch_bytes(alpha, &series)
}

/// The range a DDSketch is costed for when its values declare none is the
/// one `assumed_magnitude_range` names, and only when the sketch is costed
/// over a range: at α = 0.01 with no declared range it is [1, 1e9]; a
/// declared range, another sketch, or α = 1.5 (costed as an empty sketch)
/// assumes none.
#[test]
fn the_assumed_ddsketch_range_is_named_only_when_the_bytes_use_it() {
    let rows = |value_magnitude_range| StateInputRows {
        folded_rows_per_state: Some((600) as f64),
        non_negative: true,
        value_magnitude_range,
        distinct_values: None,
    };
    let declared = ValueMagnitudeRange {
        min_abs: 0.001,
        max_abs: 10_000.0,
    };
    let ddsketch = |alpha| SketchParams::DDSketch { alpha };
    assert_eq!(
        assumed_magnitude_range(&ddsketch(0.01), rows(None)),
        Some(DDSKETCH_ASSUMED_MAGNITUDE_RANGE)
    );
    // Undeclared, 600 rows are charged a wide spread within the assumed
    // range, at most the range itself.
    assert!(
        sketch_state_bytes(&ddsketch(0.01), rows(None))
            <= sketch_state_bytes(
                &ddsketch(0.01),
                rows(Some(DDSKETCH_ASSUMED_MAGNITUDE_RANGE))
            )
    );
    assert_eq!(
        assumed_magnitude_range(&ddsketch(0.01), rows(Some(declared))),
        None
    );
    assert_eq!(assumed_magnitude_range(&ddsketch(1.5), rows(None)), None);
    assert_eq!(
        sketch_state_bytes(&ddsketch(1.5), rows(None)),
        std::mem::size_of::<asap_sketchlib::DdSketch>() as u64
    );
    assert_eq!(
        assumed_magnitude_range(&SketchParams::Kll { k: 200 }, rows(None)),
        None
    );
}

/// A KLL state is charged what a sealed `asap_sketchlib` sketch holds for
/// the samples it saw (the executor shrinks each sketch when it seals a
/// window): the struct, the boundaries of the levels in use and the
/// retained items. E.g. a 10 s tumbling window of a series sampled every
/// 15 s sees one sample: 248 + 16 + 8 = 272 bytes, where an open sketch
/// holds 872.
#[test]
fn kll_state_bytes_grow_with_the_samples_it_sees() {
    let k = 269;
    // The library's bytes after `n` samples, shrunk as sealed.
    let library_bytes = |n: u32| {
        let mut sketch = asap_sketchlib::Kll::<f64>::with_k(k as i32);
        for i in 0..n {
            sketch.update(&f64::from(i));
        }
        sketch.shrink_to_fit();
        sketch.memory_bytes() as u64
    };
    let model_bytes = |rows_per_state: Option<u64>| {
        sketch_state_bytes(
            &SketchParams::Kll { k },
            StateInputRows {
                folded_rows_per_state: rows_per_state.map(|rows: u64| rows as f64),
                non_negative: true,
                value_magnitude_range: None,
                distinct_values: None,
            },
        )
    };
    assert_eq!(library_bytes(1), 272);
    // Up to `k` samples nothing compacts, so the sealed sketch holds
    // exactly the struct, two level boundaries and the samples.
    for n in [1, 4, 16, 17, 100, k] {
        assert_eq!(
            model_bytes(Some(u64::from(n))),
            library_bytes(n),
            "{n} samples"
        );
    }
    // Past `k`, the open sketch's grown buffer: at least what the sealed
    // sketch retains.
    for n in [k + 1, 1_000, 5_000, 100_000] {
        assert!(
            library_bytes(n) <= model_bytes(Some(u64::from(n))),
            "{n} samples"
        );
    }
    // 5,000 rows fill 6 levels: 7 boundaries.
    assert_eq!(model_bytes(Some(5_000)), 248 + 8 * 7 + 8 * 1_206);
    // A state assumed to see no sample holds at least one; unknown
    // samples fill every level to its capacity.
    assert_eq!(model_bytes(Some(0)), 272);
    assert_eq!(model_bytes(None), 248 + 8 * 62 + 8 * 1_206);
}

/// Plan selection prices each state the executor builds per group at
/// what the executor charges for it (#947). Each family is built
/// through the executor's factory and fed `n` rows shaped as the
/// pricing assumes: signed values over [`DDSKETCH_ASSUMED_MAGNITUDE_RANGE`]
/// (signed latency-like values for DDSketch), label values of
/// [`ASSUMED_VALUE_KEY_BYTES`] keys for UnivMon, or
/// keyed rows whose tuple keys are [`ASSUMED_HEAP_KEY_BYTES`] long. The
/// heap families' shapes are dominated by their heaps. The charge is
/// the sketch's bytes plus the accumulator's own fields, named per
/// family. It equals the priced bytes where `asap_sketchlib`'s estimate
/// is exact (Count-Min, Count Sketch, HLL, KMV at `n` distinct items) and
/// for KLL up to `k` rows. DDSketch is priced at what it is expected to
/// hold, within 1.5 times either way. For the heap sketches and UnivMon
/// the price bounds it, by at most [`BYTES_AGREEMENT_FACTOR`] times once
/// the state is full;
/// UnivMon's price also bounds the bytes its sketch holds.
/// KLL past `k` rows is priced at an open sketch's grown buffer, at least
/// what the sealed (shrunk) sketch holds.
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
    use std::mem::{align_of, size_of};
    use SketchAlgorithm as A;
    /// How the priced bytes relate to the sketch's.
    enum Priced {
        Exact,
        /// Exact up to `k` rows (sealed); past them at most the price
        /// (an open sketch's grown buffer).
        Kll {
            k: u32,
        },
        /// At least, and at most [`BYTES_AGREEMENT_FACTOR`] times once the
        /// state is full.
        Bound,
        /// At least the stored bytes, at most 4.2 times them: a DDSketch
        /// with no declared range is priced over the assumed nine decades
        /// (here latency-like values), its count width bounded by the
        /// rows.
        WideMargin,
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
            A::KLL,
            SketchParams::Kll { k: 200 },
            Rows::Values,
            Priced::Kll { k: 200 },
            0,
            10_000,
        ),
        (
            A::DDSketch,
            SketchParams::DDSketch { alpha: 0.01 },
            Rows::LatencyLike,
            Priced::WideMargin,
            0,
            10_000,
        ),
        (
            A::CountMinSketch,
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
            A::CountMinSketchWithHeap,
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
            A::HLL,
            SketchParams::Hll { precision: 12 },
            Rows::Values,
            Priced::Exact,
            hll_precision_tag,
            10_000,
        ),
        (
            A::KMV,
            SketchParams::Kmv { k: 1_024 },
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
            },
            Rows::Labels,
            Priced::Bound,
            univmon_key_bounds,
            4_096,
        ),
    ];
    assert_eq!(
        Value::Utf8(label(0).into()).key().unwrap().len(),
        ASSUMED_VALUE_KEY_BYTES
    );
    let key = asap_sketchlib::TupleKey::new(
        labels(0)
            .into_iter()
            .map(asap_sketchlib::KeyColumn::String)
            .collect(),
    );
    assert_eq!(key.to_bytes().unwrap().len(), ASSUMED_HEAP_KEY_BYTES);
    for (algorithm, params, rows, priced_as, fields, full) in &families {
        for n in [1, 100, *full] {
            let family = FieldDataType::Sketch(*algorithm);
            let mut updater =
                create_planner_accumulator(&family, Some(params), &rows.update()).unwrap();
            let latency = latency_like(0, n);
            let mut fed = Vec::new();
            for i in 0..n {
                match rows {
                    // Distinct magnitudes over the assumed range, in a
                    // scrambled order, alternating in sign.
                    Rows::Values => {
                        let at = f64::from(i * 7_919 % n) / f64::from(n);
                        let magnitude = DDSKETCH_ASSUMED_MAGNITUDE_RANGE.max_abs.powf(at);
                        let sign = if i % 2 == 0 { 1.0 } else { -1.0 };
                        fed.push(sign * magnitude);
                        updater.update_single(sign * magnitude, 0).unwrap();
                    }
                    Rows::LatencyLike => {
                        let magnitude = latency[i as usize];
                        let sign = if i % 2 == 0 { 1.0 } else { -1.0 };
                        fed.push(sign * magnitude);
                        updater.update_single(sign * magnitude, 0).unwrap();
                    }
                    Rows::Labels => updater
                        .update_value(&Value::Utf8(label(i).into()), 0)
                        .unwrap(),
                    Rows::Keyed => updater.update_keyed(&labels(i), 1.0, 0).unwrap(),
                }
            }
            // As the executor seals a window: a DDSketch into its view
            // layout, any other sketch shrunk.
            updater.shrink_to_fit();
            let state = updater.into_accumulator();
            let charged = match params {
                SketchParams::DDSketch { alpha } => {
                    sealed_ddsketch_bytes(*alpha, &[fed]).round() as usize
                }
                _ => state.approx_memory_bytes(),
            };
            let priced = fields
                + sketch_state_bytes(
                    params,
                    StateInputRows {
                        folded_rows_per_state: Some((u64::from(n)) as f64),
                        non_negative: false,
                        value_magnitude_range: None,
                        distinct_values: None,
                    },
                ) as usize;
            let context = format!("{algorithm:?} after {n} rows: {priced} priced");
            match priced_as {
                Priced::Exact => assert_eq!(charged, priced, "{context}"),
                Priced::Kll { k } if n <= *k => assert_eq!(charged, priced, "{context}"),
                // Past `k` rows, the open buffer: at least the sealed sketch.
                Priced::Kll { .. } => assert!(charged <= priced, "{context}, {charged} charged"),
                // One value can be a zero, which no store holds.
                Priced::WideMargin if n == 1 => assert!(charged <= priced, "{context}"),
                Priced::WideMargin => assert!(
                    (1.0..=4.2).contains(&(priced as f64 / charged as f64)),
                    "{context}, {charged} charged"
                ),
                Priced::Bound => {
                    assert!(charged <= priced, "{context}, {charged} charged");
                    if n == *full {
                        assert!(
                            priced as f64 <= BYTES_AGREEMENT_FACTOR * charged as f64,
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
            // here), so this bound is 1.6 times rather than
            // `BYTES_AGREEMENT_FACTOR`.
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

/// How far plan selection's bytes for a state may be from what the
/// executor charges for it: priced ÷ charged in `[1 / 1.5, 1.5]` on
/// seeded data ([`priced_bytes_agree_with_the_executor`]), and priced at
/// most 1.5 times the charge once a worst-case state is full
/// ([`state_bytes_bound_the_executors_charge`]).
const BYTES_AGREEMENT_FACTOR: f64 = 1.5;

/// A family's priced bytes known to disagree with the executor's beyond
/// [`BYTES_AGREEMENT_FACTOR`], on its row of
/// [`priced_bytes_agree_with_the_executor`]: the issue that fixes it and
/// priced ÷ charged as measured there. The test fails when the family
/// agrees (remove the gap) or its ratio moves more than 5% off this one.
struct KnownBytesGap {
    issue: u32,
    priced_over_charged: f64,
}

/// The next value of the seeded stream `state` (splitmix64), uniform in
/// `[0, 1)`, so the rows every family is fed are fixed.
fn next_uniform(state: &mut u64) -> f64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
}

/// Plan selection prices each state at what the executor charges for it
/// on data shaped like a workload's, not only on the worst case
/// [`state_bytes_bound_the_executors_charge`] feeds. Every family the
/// executor builds from a plan, at Pass 1's parameters for a 1% error, is
/// built through the executor and fed seeded rows ([`Rows::fed`]):
/// 10,000 latencies log-uniform over 1 ms to 1 s (declared
/// non-negative), or 1,000 rows of 1,000 distinct label values or label
/// sets. Its charge as the executor seals it (a sketch shrunk, its
/// `approx_memory_bytes`, but KLL, whose 10,000 rows are past `k` and
/// priced open; an exact family's sealed column of 1,024 groups,
/// per group; for HydraCountMinSketch its shared grid over its 10 groups) is compared
/// with `summary_state_bytes` for the same rows. Priced ÷
/// charged lies within [`BYTES_AGREEMENT_FACTOR`], or the family's row
/// carries its [`KnownBytesGap`].
///
/// HydraKLL is left out: no plan builds it (it has no parameter list,
/// and the executor's factory refuses it).
#[test]
fn priced_bytes_agree_with_the_executor() {
    use asap_executor::factory::{create_hydra_cms, create_planner_accumulator};
    use asap_executor::values::Value;
    use asap_types::summary_type::{ExactKind, ExactParams, HydraParams};
    use SketchAlgorithm as A;
    const DISTINCT_KEYS: u32 = 1_000;
    const HYDRA_GROUPS: u32 = 10;
    let input_rows = |rows_per_state: u32| StateInputRows {
        folded_rows_per_state: Some((u64::from(rows_per_state)) as f64),
        non_negative: true,
        value_magnitude_range: None,
        distinct_values: None,
    };
    let gap = |issue, priced_over_charged| {
        Some(KnownBytesGap {
            issue,
            priced_over_charged,
        })
    };
    let sketch = FieldDataType::Sketch;
    // Priced as one record of the executor's sealed exact state column,
    // and charged as that column's bytes per group.
    let exact = |kind, params| {
        (
            FieldDataType::ExactAggregate(kind, params),
            None,
            Rows::Values,
            None,
        )
    };
    // (family, its parameters, the rows it is fed, its known gap).
    let families = [
        // The rows are seeded, but the executor builds KLL unseeded
        // (`Kll::with_k`, no seed through its factory), so its compaction
        // coin differs per run: the charge varies by about 0.2% (e.g.
        // 10,376 or 10,392 bytes), well inside `BYTES_AGREEMENT_FACTOR`.
        (
            sketch(A::KLL),
            Some(SketchParams::Kll { k: 269 }),
            Rows::Values,
            None,
        ),
        (
            sketch(A::HLL),
            Some(SketchParams::Hll { precision: 18 }),
            Rows::Values,
            None,
        ),
        (
            sketch(A::KMV),
            Some(SketchParams::Kmv { k: 106_263 }),
            Rows::Values,
            None,
        ),
        (
            sketch(A::CountMinSketch),
            Some(SketchParams::Cms {
                width: 272,
                depth: 5,
            }),
            Rows::Keyed,
            None,
        ),
        (
            sketch(A::CountSketch),
            Some(SketchParams::CountSketch {
                width: 30_000,
                depth: 47,
            }),
            Rows::Keyed,
            None,
        ),
        (
            sketch(A::CountMinSketchWithHeap),
            Some(SketchParams::CmsWithHeap {
                width: 272,
                depth: 5,
                heap_size: 10,
            }),
            Rows::Keyed,
            None,
        ),
        (
            sketch(A::CountSketchWithHeap),
            Some(SketchParams::CountSketchWithHeap {
                width: 30_000,
                depth: 47,
                heap_size: 10,
            }),
            Rows::Keyed,
            None,
        ),
        (
            sketch(A::UnivMon),
            Some(SketchParams::UnivMon {
                heap_size: 256,
                sketch_rows: 5,
                sketch_cols: 1_024,
                layers: 16,
            }),
            Rows::Labels,
            None,
        ),
        exact(ExactKind::Sum, ExactParams::Sum),
        exact(ExactKind::Count, ExactParams::Count),
        exact(ExactKind::Min, ExactParams::Min),
        exact(ExactKind::Max, ExactParams::Max),
        exact(ExactKind::Counter, ExactParams::Counter),
    ];
    // Every family the executor declares is checked: here, or DDSketch
    // against its own bands below.
    for supported in asap_executor::supported_summary_families() {
        assert!(
            supported.summary_type == A::DDSketch
                || families
                    .iter()
                    .any(|(family, ..)| *family == sketch(supported.summary_type)),
            "{:?} is built by the executor but not checked here",
            supported.summary_type
        );
    }
    // (family, priced bytes, charged bytes, its known gap).
    let mut measured = Vec::new();
    for (family, params, rows, known_gap) in families {
        let rows_fed = rows.fed();
        if let FieldDataType::ExactAggregate(..) = family {
            // A sealed column of 1,024 groups, the rows spread over them.
            const GROUPS: u32 = 1_024;
            let mut updater = asap_executor::StateColumnUpdater::new(&family, None).unwrap();
            updater.ensure_groups(GROUPS as usize).unwrap();
            let mut seed = 0x5EED_u64;
            for i in 0..rows_fed {
                let latency_ms = 1_000f64.powf(next_uniform(&mut seed));
                updater
                    .update(i % GROUPS, latency_ms, i64::from(i) * 1_000)
                    .unwrap();
            }
            let charged = (updater.seal().state_memory_bytes() / GROUPS as usize) as u64;
            let priced = summary_state_bytes(&family, None, input_rows(rows_fed / GROUPS)).unwrap();
            measured.push((family, priced, charged, known_gap));
            continue;
        }
        let mut updater =
            create_planner_accumulator(&family, params.as_ref(), &rows.update()).unwrap();
        let mut seed = 0x5EED_u64;
        let latency = latency_like(0, rows_fed);
        for i in 0..rows_fed {
            let key = (next_uniform(&mut seed) * f64::from(DISTINCT_KEYS)) as u32;
            match rows {
                // One sample a second, its latency log-uniform over 1 ms to 1 s.
                Rows::Values => {
                    let latency_ms = 1_000f64.powf(next_uniform(&mut seed));
                    updater
                        .update_single(latency_ms, i64::from(i) * 1_000)
                        .unwrap();
                }
                Rows::LatencyLike => updater
                    .update_single(latency[i as usize], i64::from(i) * 1_000)
                    .unwrap(),
                Rows::Labels => updater
                    .update_value(&Value::Utf8(label(key).into()), 0)
                    .unwrap(),
                Rows::Keyed => updater.update_keyed(&labels(key), 1.0, 0).unwrap(),
            }
        }
        // As the executor seals a window, except a KLL past `k` rows,
        // which is priced as the open sketch a query-time merge holds
        // (`kll_sealed_bytes`).
        if !matches!(family, FieldDataType::Sketch(A::KLL)) {
            updater.shrink_to_fit();
        }
        let charged = updater.into_accumulator().approx_memory_bytes() as u64;
        let priced = summary_state_bytes(&family, params.as_ref(), input_rows(rows_fed)).unwrap();
        measured.push((family, priced, charged, known_gap));
    }
    // DDSketch, with no declared range, is priced dense over the assumed
    // nine decades at the count width its rows bound (2 bytes for 10,000):
    // fed latency-like rows (an exponential spread) or the log-uniform
    // three decades of `Rows::Values`, whose counts fit a byte, the sealed
    // column holds less, within these bands.
    for (rows, band) in [
        (Rows::LatencyLike, 3.328..=3.678),
        (Rows::Values, 4.647..=5.137),
    ] {
        let family = sketch(A::DDSketch);
        let params = SketchParams::DDSketch { alpha: 0.01 };
        let mut updater =
            create_planner_accumulator(&family, Some(&params), &rows.update()).unwrap();
        let mut seed = 0x5EED_u64;
        let latency = latency_like(0, rows.fed());
        let mut fed = Vec::new();
        for i in 0..rows.fed() {
            let value = match rows {
                Rows::LatencyLike => latency[i as usize],
                _ => 1_000f64.powf(next_uniform(&mut seed)),
            };
            fed.push(value);
            updater.update_single(value, i64::from(i) * 1_000).unwrap();
        }
        drop(updater);
        // What the executor holds for the sealed window.
        let charged = sealed_ddsketch_bytes(0.01, &[fed]);
        let priced =
            summary_state_bytes(&family, Some(&params), input_rows(rows.fed())).unwrap() as f64;
        assert!(
            band.contains(&(priced / charged)),
            "DDSketch over {rows:?}: {priced} priced, {charged} charged, ratio {:.3}, band {band:?}",
            priced / charged
        );
    }
    // HydraCms: one grid for every group, priced as one Count-Min per
    // group; the executor keeps one grid for all of them, here 3 × 64
    // cells over 10 groups.
    let hydra = SketchParams::Hydra(HydraParams::HydraCms {
        width: 272,
        depth: 5,
        shared_rows: 3,
        shared_columns: 64,
    });
    let hydra_family = sketch(A::HydraCountMinSketch);
    let mut grid = create_hydra_cms(&hydra_family, Some(&hydra)).unwrap();
    let hydra_rows_fed = Rows::Keyed.fed();
    let mut seed = 0x5EED_u64;
    for _ in 0..hydra_rows_fed {
        let group = (next_uniform(&mut seed) * f64::from(HYDRA_GROUPS)) as u32;
        let item = (next_uniform(&mut seed) * f64::from(DISTINCT_KEYS)) as u32;
        grid.count(&label(group), &label(item), 1).unwrap();
    }
    measured.push((
        hydra_family,
        sketch_state_bytes(&hydra, input_rows(hydra_rows_fed / HYDRA_GROUPS)),
        (grid.approx_memory_bytes() / HYDRA_GROUPS as usize) as u64,
        gap(947, 0.104),
    ));

    let mut failures = Vec::new();
    for (family, priced, charged, known_gap) in &measured {
        let ratio = *priced as f64 / *charged as f64;
        let agrees = (1.0 / BYTES_AGREEMENT_FACTOR..=BYTES_AGREEMENT_FACTOR).contains(&ratio);
        let line = format!(
            "{}: {priced} priced, {charged} charged, ratio {ratio:.3}",
            crate::node::usage::summary_type_name(family)
        );
        match known_gap {
            Some(KnownBytesGap { issue, .. }) if agrees => {
                failures.push(format!("{line}: agrees now; remove its known gap (#{issue})"))
            }
            Some(KnownBytesGap {
                issue,
                priced_over_charged,
            }) if (ratio / priced_over_charged - 1.0).abs() > 0.05 => failures.push(format!(
                "{line}: its known gap (#{issue}) is {priced_over_charged}; update it"
            )),
            Some(_) => {}
            None if !agrees => failures.push(format!(
                "{line}: outside {BYTES_AGREEMENT_FACTOR}x; fix it or give its row a known gap with its issue"
            )),
            None => {}
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A per-series state's row holds, beside the state, its offset and a key
/// per text column (4 + 2 × 4 = 12 bytes at the defaults), and no label
/// text; the state's own bytes, which a merge is costed by, leave them out.
/// A stored per-series Sum carrying `job`: 36 bytes per row, 24 of them its
/// state.
#[test]
fn state_bytes_per_row_leave_out_what_the_row_holds_beside_its_state() {
    use asap_types::ir::schema::{Field, PROMQL_SERIES_IDENTITY};
    use asap_types::summary_type::{ExactKind, ExactParams};
    let schema = Schema::new(vec![
        Field::new("ts", FieldDataType::Plain(DataType::Int64), false),
        Field::new(
            "state",
            FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
            false,
        ),
        Field::new("job", FieldDataType::Plain(DataType::Utf8), false),
        Field::new(
            PROMQL_SERIES_IDENTITY,
            FieldDataType::Plain(DataType::Utf8),
            false,
        ),
    ]);
    let execution = crate::DataPlaneExecution::default();
    assert_eq!(state_row_bytes_beside_state(&schema, &execution), 12);
    let edge = EdgeStatistics {
        rows: 1_000,
        bytes: 36_000,
    };
    assert_eq!(state_bytes_per_row(edge, &schema, &execution), 24);
}

/// `n` values of series `series` with a wide floor: a lognormal of median
/// 10 and σ = 2.
fn lognormal_sigma_2(series: u64, n: u32) -> Vec<f64> {
    lognormal(series, n)
        .into_iter()
        .map(|v| 10.0 * (v / 10.0).powi(2))
        .collect()
}

/// `n` Zipf(1.1) integers in [1, 1e6] of series `series`, by inverting the
/// continuous power law `P(X > x) ∝ x^-0.1` over the range, rounded down.
fn zipf_integers(series: u64, n: u32) -> Vec<f64> {
    let (s, max) = (1.1_f64, 1e6_f64);
    let a = 1.0 - s;
    let top = max.powf(a);
    (0..u64::from(n))
        .map(|i| {
            let u = uniform((series << 32) ^ i);
            (1.0 + u * (top - 1.0))
                .powf(1.0 / a)
                .floor()
                .clamp(1.0, max)
        })
        .collect()
}

/// `n` readings of a counter of series `series` near 1e6, one increment of
/// 0 to 10 per reading.
fn counter_near_a_million(series: u64, n: u32) -> Vec<f64> {
    let mut value = 1e6 + (series * 1_000) as f64;
    (0..u64::from(n))
        .map(|i| {
            value += (uniform((series << 32) ^ i) * 10.0).floor();
            value
        })
        .collect()
}

/// A stored DDSketch state is charged, against the view layout an
/// `asap_sketchlib` sketch is stored in (the mean of 20 series), within
/// these bands at α = 0.01 and 10 to 36,000 values per state, the table in
/// `ddsketch_state_bytes`' documentation. No shape is under-charged by
/// more than the rounding of the occupied-bucket estimate (few rows are
/// stored sparse, priced closely); the count width is bounded by the rows,
/// which over-charges 256 to 65,535 rows whose largest count fits a byte
/// up to about 2x (latency-like values in the generator's declared range
/// 4.9x at 600 rows); narrow values without a declared range (a gauge, a
/// counter's readings, per-series levels) are over-charged by the
/// wide-spread margin's span, up to 28x.
#[test]
fn ddsketch_state_bytes_by_value_shape_lie_within_their_bands() {
    let alpha = 0.01;
    let range = |min_abs, max_abs| Some(ValueMagnitudeRange { min_abs, max_abs });
    type Shape = fn(u64, u32) -> Vec<f64>;
    let shapes: [(
        &str,
        Shape,
        Option<ValueMagnitudeRange>,
        std::ops::RangeInclusive<f64>,
    ); 13] = [
        ("exponential", latency_like, None, 0.95..=4.3),
        (
            "exponential, the generator's range declared",
            latency_like,
            range(2.687e-8, 1_104.2),
            0.95..=5.0,
        ),
        ("lognormal sigma 1", lognormal, None, 0.95..=5.65),
        ("lognormal sigma 2", lognormal_sigma_2, None, 0.95..=3.2),
        (
            "log-uniform 7 decades",
            log_uniform_over_seven_decades,
            None,
            0.95..=2.55,
        ),
        (
            "log-uniform 7 decades, declared",
            log_uniform_over_seven_decades,
            range(0.001, 1e4),
            0.95..=2.0,
        ),
        ("Zipf(1.1) integers", zipf_integers, None, 0.97..=2.95),
        (
            "Zipf(1.1) integers, declared",
            zipf_integers,
            range(1.0, 1e6),
            0.95..=2.0,
        ),
        ("narrow gauge", narrow_gauge, None, 1.3..=26.0),
        (
            "narrow gauge, declared",
            narrow_gauge,
            range(90.0, 110.0),
            0.95..=1.15,
        ),
        ("counter near 1e6", counter_near_a_million, None, 1.4..=28.7),
        ("per-series levels", per_series_levels, None, 1.3..=26.0),
        (
            "per-series levels, column range declared",
            per_series_levels,
            range(1.0, 1e5),
            1.3..=14.9,
        ),
    ];
    for (name, values, value_magnitude_range, band) in shapes {
        for n in [10_u32, 100, 600, 3_000, 36_000] {
            let charged = sketch_state_bytes(
                &SketchParams::DDSketch { alpha },
                StateInputRows {
                    folded_rows_per_state: Some((u64::from(n)) as f64),
                    non_negative: true,
                    value_magnitude_range,
                    distinct_values: None,
                },
            );
            let held = library_ddsketch_mean_bytes(alpha, n, false, values);
            let ratio = charged as f64 / held;
            assert!(
                band.contains(&ratio),
                "{name}, {n} values: charged {charged}, held {held} ({ratio:.2}x), band {band:?}"
            );
        }
    }
}

/// The view header the price charges is the library's: a one-value
/// DDSketch's view bytes (header and one 1-byte count padded to 8), plus
/// the state's 4-byte offset in its column, are what a one-row state is
/// priced at.
#[test]
fn stored_ddsketch_header_is_the_librarys_view_header() {
    let alpha = 0.01;
    let mut sketch = asap_sketchlib::DdSketch::new(alpha);
    sketch.add(&42.0);
    let held = sketch.to_view_bytes().len() as u64 + 4;
    let priced = sketch_state_bytes(
        &SketchParams::DDSketch { alpha },
        StateInputRows {
            folded_rows_per_state: Some(1.0),
            non_negative: true,
            value_magnitude_range: None,
            distinct_values: None,
        },
    );
    assert_eq!(priced, held, "priced {priced} B, view layout {held} B");
}

/// A stored DDSketch is priced at its view layout and its offset in the
/// column (64 + 4 bytes), then the occupied buckets dense over the
/// column's range (declared, or the assumed nine decades; plus one bucket
/// for an unaligned start) or sparse at one bucket per row (4-byte index
/// and count each), whichever is smaller, at the narrowest count width the
/// rows allow; each array padded to 8 bytes; two stores for signed values.
#[test]
fn stored_ddsketch_states_are_priced_at_their_view_layout() {
    let alpha = 0.01;
    let priced = |rows_per_state: Option<u64>, non_negative, value_magnitude_range| {
        sketch_state_bytes(
            &SketchParams::DDSketch { alpha },
            StateInputRows {
                folded_rows_per_state: rows_per_state.map(|rows: u64| rows as f64),
                non_negative,
                value_magnitude_range,
                distinct_values: None,
            },
        )
    };
    let gauge = Some(ValueMagnitudeRange {
        min_abs: 90.0,
        max_abs: 110.0,
    });
    // One row: one bucket, a 1-byte count dense (padded to 8).
    assert_eq!(priced(Some(1), true, None), 68 + 8);
    // 10 rows over the assumed nine decades: sparse, 10 buckets at most.
    assert_eq!(priced(Some(10), true, None), 68 + 40 + 16);
    // A gauge's 12 buckets, dense: 1, 2 and 2 bytes per count.
    assert_eq!(priced(Some(10), true, gauge), 68 + 16);
    assert_eq!(priced(Some(600), true, gauge), 68 + 24);
    assert_eq!(priced(Some(36_000), true, gauge), 68 + 24);
    // 4 bytes up to u32::MAX rows, then 8 (2^33: past u32::MAX once the
    // rows are rounded within their relative 1e-9, `rows_per_state`).
    assert_eq!(priced(Some(u64::from(u32::MAX)), true, gauge), 68 + 48);
    assert_eq!(priced(Some(1 << 33), true, gauge), 68 + 96);
    assert!(priced(Some(10), true, None) < priced(Some(100), true, None));
    // Unknown rows: the assumed range, dense at 8 bytes.
    let unknown = priced(None, true, None);
    assert_eq!((unknown - 68) % 8, 0);
    assert!(unknown > priced(Some(1_000_000), true, None));
    // Signed values fill two stores.
    assert_eq!(
        priced(Some(20_000), false, None) - 68,
        2 * (priced(Some(10_000), true, None) - 68)
    );
}

/// A DDSketch still receiving values (a chain's current window, a
/// query-time build, a merge target) is priced open, as the owned sketch
/// it is: its struct, and per store one 128-slot first block (1 KiB) plus
/// an 8-byte count per bucket beyond the first, above the stored layout;
/// every other family is priced the same open or stored.
#[test]
fn open_ddsketch_states_hold_their_first_blocks() {
    let rows = |non_negative| StateInputRows {
        folded_rows_per_state: Some((100) as f64),
        non_negative,
        value_magnitude_range: None,
        distinct_values: None,
    };
    let ddsketch = SketchParams::DDSketch { alpha: 0.01 };
    let struct_bytes = std::mem::size_of::<asap_sketchlib::DdSketch>() as u64;
    for non_negative in [true, false] {
        let open = open_sketch_state_bytes(&ddsketch, rows(non_negative));
        assert!(open > sketch_state_bytes(&ddsketch, rows(non_negative)));
        assert_eq!((open - struct_bytes) % 8, 0);
    }
    let kll = SketchParams::Kll { k: 200 };
    assert_eq!(
        open_sketch_state_bytes(&kll, rows(true)),
        sketch_state_bytes(&kll, rows(true))
    );
}

/// `n` values of series `series` around its own level, the levels of the
/// series spread log-uniformly over [1, 1e5]: each value its level ± 10%.
fn per_series_levels(series: u64, n: u32) -> Vec<f64> {
    let level = 10f64.powf(5.0 * uniform(series.wrapping_mul(0x9e37_79b9)));
    (0..u64::from(n))
        .map(|i| level * (0.9 + 0.2 * uniform((series << 32) ^ i)))
        .collect()
}

/// A constant: every value 42.
fn constant(_series: u64, n: u32) -> Vec<f64> {
    vec![42.0; n as usize]
}

/// Pareto values of shape 1.1 and scale 1 (a heavy tail), capped at 1e12.
fn pareto(series: u64, n: u32) -> Vec<f64> {
    (0..u64::from(n))
        .map(|i| {
            (1.0 - uniform((series << 32) ^ i))
                .powf(-1.0 / 1.1)
                .min(1e12)
        })
        .collect()
}

/// Uniform in [0, 1000).
fn uniform_to_a_thousand(series: u64, n: u32) -> Vec<f64> {
    (0..u64::from(n))
        .map(|i| 1_000.0 * uniform((series << 32) ^ i))
        .collect()
}

/// Two modes: 1 ms and 1 s, each ± 5%.
fn bimodal(series: u64, n: u32) -> Vec<f64> {
    (0..u64::from(n))
        .map(|i| {
            let u = uniform((series << 32) ^ i);
            let mode = if u < 0.5 { 0.001 } else { 1.0 };
            mode * (0.95 + 0.2 * (u - if u < 0.5 { 0.0 } else { 0.5 }))
        })
        .collect()
}

/// Small integer counts 0..=5, zeros included.
fn small_counts(series: u64, n: u32) -> Vec<f64> {
    (0..u64::from(n))
        .map(|i| (6.0 * uniform((series << 32) ^ i)).floor())
        .collect()
}

/// Lognormal values of median 1e-6 and σ = 1 (sub-microsecond scale).
fn tiny_lognormal(series: u64, n: u32) -> Vec<f64> {
    lognormal(series, n).into_iter().map(|v| v * 1e-7).collect()
}

/// Log-uniform over fifteen decades, [1e-6, 1e9].
fn log_uniform_over_fifteen_decades(series: u64, n: u32) -> Vec<f64> {
    (0..u64::from(n))
        .map(|i| 1e-6 * 10f64.powf(15.0 * uniform((series << 32) ^ i)))
        .collect()
}

/// A stored DDSketch state is never priced below 0.99 of what the
/// executor holds for it sealed (its view layout and offset), over α from
/// 0.001 to 0.1, 1 to 140,000 rows (both sides of the 1- and 2-byte count
/// widths), unsigned and signed values, 15 value shapes, each with no
/// declared range and, where the shape has a natural one, declared.
#[test]
fn stored_ddsketch_prices_never_fall_below_what_the_executor_holds() {
    type Shape = fn(u64, u32) -> Vec<f64>;
    let range = |min_abs, max_abs| Some(ValueMagnitudeRange { min_abs, max_abs });
    // The last field: whether the values fit the range assumed when none
    // is declared (nine decades); a shape that does not is tested declared
    // only.
    let shapes: [(&str, Shape, Option<ValueMagnitudeRange>, bool); 15] = [
        ("exponential", latency_like, range(2.687e-8, 1_104.2), true),
        ("lognormal sigma 1", lognormal, None, true),
        ("lognormal sigma 2", lognormal_sigma_2, None, true),
        ("narrow gauge", narrow_gauge, range(90.0, 110.0), true),
        (
            "log-uniform 7 decades",
            log_uniform_over_seven_decades,
            range(0.001, 1e4),
            true,
        ),
        ("Zipf(1.1) integers", zipf_integers, range(1.0, 1e6), true),
        ("counter near 1e6", counter_near_a_million, None, true),
        (
            "per-series levels",
            per_series_levels,
            range(1.0, 1e5),
            true,
        ),
        ("constant", constant, range(42.0, 42.0), true),
        ("Pareto(1.1)", pareto, range(1.0, 1e12), true),
        (
            "uniform to 1000",
            uniform_to_a_thousand,
            range(1e-6, 1_000.0),
            true,
        ),
        ("bimodal 1 ms / 1 s", bimodal, range(9e-4, 1.1), true),
        ("small counts", small_counts, range(1.0, 5.0), true),
        ("tiny lognormal", tiny_lognormal, None, true),
        (
            "log-uniform 15 decades",
            log_uniform_over_fifteen_decades,
            range(1e-6, 1e9),
            false,
        ),
    ];
    let rows = [
        1_u32, 2, 10, 100, 255, 256, 1_000, 10_000, 65_535, 65_536, 140_000,
    ];
    let mut lowest = (f64::INFINITY, String::new());
    for alpha in [0.001, 0.01, 0.1] {
        for (name, values, declared, within_assumed) in shapes {
            for n in rows {
                for signed in [false, true] {
                    let series: Vec<Vec<f64>> = (0..if n >= 10_000 { 2 } else { 4 })
                        .map(|s| {
                            values(s, n)
                                .into_iter()
                                .enumerate()
                                .map(|(i, v)| if signed && i % 2 == 0 { -v } else { v })
                                .collect()
                        })
                        .collect();
                    let held = sealed_ddsketch_bytes(alpha, &series);
                    for value_magnitude_range in [None, declared] {
                        if value_magnitude_range.is_none() && !within_assumed {
                            continue;
                        }
                        let priced = sketch_state_bytes(
                            &SketchParams::DDSketch { alpha },
                            StateInputRows {
                                folded_rows_per_state: Some(f64::from(n)),
                                non_negative: !signed,
                                value_magnitude_range,
                                distinct_values: None,
                            },
                        ) as f64;
                        let ratio = priced / held;
                        if ratio < lowest.0 {
                            lowest = (
                                ratio,
                                format!(
                                    "{name}, alpha {alpha}, {n} rows, signed {signed}, declared {value_magnitude_range:?}: priced {priced}, held {held}"
                                ),
                            );
                        }
                    }
                }
            }
        }
    }
    assert!(lowest.0 >= 0.99, "lowest {:.3}: {}", lowest.0, lowest.1);
}

/// A grouped state's row (no series identity) keeps its window's time and
/// its group keys beside the state, whatever a per-series row holds (the
/// row layout inputs, 8 and 2 here, do not apply): a Sum grouped by `job`
/// holds 8 + 16 = 24 bytes beside its state.
#[test]
fn a_grouped_state_row_keeps_its_group_keys() {
    use asap_types::summary_type::{ExactKind, ExactParams};
    let schema = Schema::new(vec![
        Field::new("ts", FieldDataType::Plain(DataType::Int64), false),
        Field::new(
            "state",
            FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
            false,
        ),
        Field::new("job", FieldDataType::Plain(DataType::Utf8), false),
    ]);
    let execution = crate::DataPlaneExecution {
        state_offset_bytes_per_row: 8,
        key_bytes_per_label_column: 2,
        ..crate::DataPlaneExecution::default()
    };
    assert_eq!(state_row_bytes_beside_state(&schema, &execution), 24);
}
