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
        rows_per_state: Some(rows_per_state),
        non_negative: true,
        value_magnitude_range: None,
        distinct_values,
    };
    let merged = StateInputRows::merged([
        Some((rows(12_000, Some(1_000)), 2)),
        Some((rows(6_000, Some(1_000_000)), 1)),
    ])
    .unwrap();
    assert_eq!(merged.rows_per_state, Some(30_000));
    assert_eq!(merged.distinct_values, Some(1_000_000));
    let merged = StateInputRows::merged([
        Some((rows(12_000, Some(1_000)), 1)),
        Some((rows(6_000, None), 1)),
    ])
    .unwrap();
    assert_eq!(merged.distinct_values, None);
}

/// The rows a family is fed.
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

/// The bytes `asap_sketchlib`'s DDSketch holds after adding `values`.
fn library_ddsketch_bytes(alpha: f64, values: &[f64]) -> u64 {
    let mut sketch = asap_sketchlib::sketches::DdSketch::new(alpha);
    for value in values {
        sketch.add(value);
    }
    sketch.memory_bytes() as u64
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
) -> u64 {
    let series = 20;
    (0..series)
        .map(|s| {
            let values: Vec<f64> = values(s, n)
                .into_iter()
                .enumerate()
                .map(|(i, v)| if signed && i % 2 == 0 { -v } else { v })
                .collect();
            library_ddsketch_bytes(alpha, &values)
        })
        .sum::<u64>()
        / series
}

/// DDSketch states are charged as if their values spread like an
/// exponential's ([`exponential_values_magnitude_ratio`]); these pin how
/// far that assumption is from other value shapes, so a change to it shows
/// here. At α = 0.01, non-negative values, 10 to 36,000 per state:
/// lognormal latencies with a floor (σ = 1) are over-charged 1.08–1.13x; a
/// narrow gauge of 100 ± 10, whose states hold one first block of buckets
/// (1,168 bytes), 2.4x at 10 values to 5.6x at 36,000.
#[test]
fn ddsketch_state_bytes_over_charge_narrower_values_within_their_bands() {
    let alpha = 0.01;
    let charged = |n: u32| {
        sketch_state_bytes(
            &SketchParams::DDSketch { alpha },
            StateInputRows {
                rows_per_state: Some(u64::from(n)),
                non_negative: true,
                value_magnitude_range: None,
                distinct_values: None,
            },
        ) as f64
    };
    for n in [10, 100, 600, 3_000, 36_000] {
        let lognormal_held = library_ddsketch_mean_bytes(alpha, n, false, lognormal) as f64;
        let ratio = charged(n) / lognormal_held;
        assert!(
            (1.0..=1.25).contains(&ratio),
            "{n} lognormal values: charged {} for {lognormal_held} held, {ratio:.2}x",
            charged(n)
        );
        let gauge_held = library_ddsketch_mean_bytes(alpha, n, false, narrow_gauge) as f64;
        let ratio = charged(n) / gauge_held;
        assert!(
            (2.0..=6.0).contains(&ratio),
            "{n} gauge values: charged {} for {gauge_held} held, {ratio:.2}x",
            charged(n)
        );
    }
}

/// A DDSketch state is charged what `asap_sketchlib` holds on average for
/// latency-like values, not the most it could hold over nine decades. At
/// α = 0.01 the executor's stored states of 1,000 latency series hold
/// 2.9 KB at 10 rows per state and 5.8 KB at 3,000, where the library's
/// worst case charged 18.8 KB for both, so stored quantile windows looked
/// 4x more expensive than they are.
#[test]
fn ddsketch_state_bytes_match_the_library_on_latency_like_values() {
    let alpha = 0.01;
    let priced = |rows_per_state, non_negative| {
        sketch_state_bytes(
            &SketchParams::DDSketch { alpha },
            StateInputRows {
                rows_per_state,
                non_negative,
                value_magnitude_range: None,
                distinct_values: None,
            },
        )
    };
    for n in [1, 10, 100, 600, 3_000, 36_000] {
        for signed in [false, true] {
            let held = library_ddsketch_mean_bytes(alpha, n, signed, latency_like);
            let charged = priced(Some(u64::from(n)), !signed);
            assert!(
                (1.0 / 1.5..=1.5).contains(&(charged as f64 / held as f64)),
                "{n} values, signed {signed}: charged {charged}, held {held} on average"
            );
        }
    }
    // Unknown rows span the assumed nine decades, more than any number of
    // latency-like values does.
    assert!(priced(None, true) > priced(Some(1_000_000), true));
    assert!(priced(Some(10_000), true) < priced(Some(10_000), false));
    assert!(priced(Some(10), true) < priced(Some(3_000), true));
}

/// The range a DDSketch is costed for when its values declare none is the
/// one `assumed_magnitude_range` names, and only when the sketch is costed
/// over a range: at α = 0.01 with no declared range it is [1, 1e9]; a
/// declared range, another sketch, or α = 1.5 (costed as an empty sketch)
/// assumes none.
#[test]
fn the_assumed_ddsketch_range_is_named_only_when_the_bytes_use_it() {
    let rows = |value_magnitude_range| StateInputRows {
        rows_per_state: Some(600),
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
    assert_eq!(
        sketch_state_bytes(&ddsketch(0.01), rows(None)),
        sketch_state_bytes(
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

/// A DDSketch's span is the smallest of the exponential spread of its
/// rows, its column's declared `column_magnitudes` and the assumed nine
/// decades. At α = 0.01, non-negative values:
/// - a narrow gauge declared within [90, 110] spans a ratio of 1.22, 11
///   buckets: 1,248 bytes at any number of rows, where `asap_sketchlib`
///   holds 1,168 (undeclared, 2,744 at 10 rows and 6,552 at 36,000);
/// - a range wider than the rows' spread (the latency generator's
///   [2.7e-8, 1104], 10.6 decades) leaves the spread's bytes unchanged;
/// - with the rows unknown, a declared [0.001, 10000] spans seven decades
///   instead of nine.
#[test]
fn ddsketch_state_bytes_are_capped_by_the_declared_value_magnitude_range() {
    let alpha = 0.01;
    let costed = |rows_per_state, value_magnitude_range| {
        sketch_state_bytes(
            &SketchParams::DDSketch { alpha },
            StateInputRows {
                rows_per_state,
                non_negative: true,
                value_magnitude_range,
                distinct_values: None,
            },
        )
    };
    let gauge = ValueMagnitudeRange {
        min_abs: 90.0,
        max_abs: 110.0,
    };
    for n in [10_u32, 600, 36_000] {
        let rows = Some(u64::from(n));
        let charged = costed(rows, Some(gauge));
        assert_eq!(charged, 1_248, "{n} gauge rows");
        let held = library_ddsketch_mean_bytes(alpha, n, false, narrow_gauge);
        assert!(
            (1.0..=1.1).contains(&(charged as f64 / held as f64)),
            "{n} gauge rows: {charged} charged, {held} held"
        );
    }
    let wide = ValueMagnitudeRange {
        min_abs: 2.687e-8,
        max_abs: 1_104.2,
    };
    for n in [10, 600, 36_000] {
        assert_eq!(
            costed(Some(n), Some(wide)),
            costed(Some(n), None),
            "{n} rows"
        );
    }
    let latency = ValueMagnitudeRange {
        min_abs: 0.001,
        max_abs: 10_000.0,
    };
    assert!(costed(None, Some(latency)) < costed(None, None));
}

/// The planner knows a state's rows, and its expected span is the
/// exponential spread of those rows, which a declared range only caps.
/// Values spread evenly in log over the whole of a declared seven decades
/// ([0.001, 10000]), in random order, span more than that and are
/// under-charged, most at few rows per state: at α = 0.01 the library
/// holds 8.2–9.5 KB from 10 rows on (the mean of 20 series), charged
/// 2,744 bytes at 10 rows per state (0.33 of it), 3,648 at 60 (0.38),
/// 4,728 at 600 (0.50), 5,536 at 3,600 (0.59) and 6,552 at 36,000 (0.70).
/// A known limit: the declared range cannot raise the span, since it
/// bounds the column, not one state's rows. Charging such a state the
/// expected span of `n` log-uniform values over the declared range
/// (`(n − 1) / (n + 1)` of its decades) would charge these 0.78–0.81 of
/// what they hold, but the latency generator's states, declared over its
/// 10.6 decades, 1.3x (36,000 rows) to 3.6x (10 rows) of theirs, where
/// they are charged 0.88–1.07 now.
#[test]
fn ddsketch_state_bytes_under_charge_log_uniform_values_over_a_declared_range() {
    let alpha = 0.01;
    let declared = ValueMagnitudeRange {
        min_abs: 0.001,
        max_abs: 10_000.0,
    };
    for (n, band) in [
        (10_u32, 0.30..=0.37),
        (60, 0.35..=0.42),
        (600, 0.46..=0.55),
        (3_600, 0.54..=0.64),
        (36_000, 0.64..=0.75),
    ] {
        let charged = sketch_state_bytes(
            &SketchParams::DDSketch { alpha },
            StateInputRows {
                rows_per_state: Some(u64::from(n)),
                non_negative: true,
                value_magnitude_range: Some(declared),
                distinct_values: None,
            },
        );
        let held = library_ddsketch_mean_bytes(alpha, n, false, log_uniform_over_seven_decades);
        let ratio = charged as f64 / held as f64;
        assert!(
            band.contains(&ratio),
            "{n} rows: {charged} charged, {held} held ({ratio:.2})"
        );
    }
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
    let model_bytes = |rows_per_state| {
        sketch_state_bytes(
            &SketchParams::Kll { k },
            StateInputRows {
                rows_per_state,
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
        /// Within 1.5 times either way.
        Expected,
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
            Priced::Expected,
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
            SketchParams::Hll { precision: 12 },
            Rows::Values,
            Priced::Exact,
            hll_precision_tag,
            10_000,
        ),
        (
            A::Kmv,
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
            for i in 0..n {
                match rows {
                    // Distinct magnitudes over the assumed range, in a
                    // scrambled order, alternating in sign.
                    Rows::Values => {
                        let at = f64::from(i * 7_919 % n) / f64::from(n);
                        let magnitude = DDSKETCH_ASSUMED_MAGNITUDE_RANGE.max_abs.powf(at);
                        let sign = if i % 2 == 0 { 1.0 } else { -1.0 };
                        updater.update_single(sign * magnitude, 0).unwrap();
                    }
                    Rows::LatencyLike => {
                        let magnitude = latency[i as usize];
                        let sign = if i % 2 == 0 { 1.0 } else { -1.0 };
                        updater.update_single(sign * magnitude, 0).unwrap();
                    }
                    Rows::Labels => updater
                        .update_value(&Value::Utf8(label(i).into()), 0)
                        .unwrap(),
                    Rows::Keyed => updater.update_keyed(&labels(i), 1.0, 0).unwrap(),
                }
            }
            // As the executor seals a window.
            updater.shrink_to_fit();
            let state = updater.into_accumulator();
            let charged = state.approx_memory_bytes();
            let priced = fields
                + sketch_state_bytes(
                    params,
                    StateInputRows {
                        rows_per_state: Some(u64::from(n)),
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
                Priced::Expected if n == 1 => assert!(charged <= priced, "{context}"),
                Priced::Expected => assert!(
                    (1.0 / 1.5..=1.5).contains(&(priced as f64 / charged as f64)),
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
/// per group; for HydraCms its shared grid over its 10 groups) is compared
/// with `summary_state_bytes` for the same rows. Priced ÷
/// charged lies within [`BYTES_AGREEMENT_FACTOR`], or the family's row
/// carries its [`KnownBytesGap`].
///
/// HydraKll is left out: no plan builds it (it has no parameter list,
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
        rows_per_state: Some(u64::from(rows_per_state)),
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
            sketch(A::Kll),
            Some(SketchParams::Kll { k: 269 }),
            Rows::Values,
            None,
        ),
        // Priced for the spread its 10,000 rows are expected to span
        // (`exponential_values_magnitude_ratio`); these span three decades.
        (
            sketch(A::DDSketch),
            Some(SketchParams::DDSketch { alpha: 0.01 }),
            Rows::Values,
            None,
        ),
        (
            sketch(A::Hll),
            Some(SketchParams::Hll { precision: 18 }),
            Rows::Values,
            None,
        ),
        (
            sketch(A::Kmv),
            Some(SketchParams::Kmv { k: 106_263 }),
            Rows::Values,
            None,
        ),
        (
            sketch(A::Cms),
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
            sketch(A::CmsWithHeap),
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
    // Every family the executor declares is checked.
    for supported in asap_executor::supported_summary_families() {
        assert!(
            families
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
        if !matches!(family, FieldDataType::Sketch(A::Kll)) {
            updater.shrink_to_fit();
        }
        let charged = updater.into_accumulator().approx_memory_bytes() as u64;
        let priced = summary_state_bytes(&family, params.as_ref(), input_rows(rows_fed)).unwrap();
        measured.push((family, priced, charged, known_gap));
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
    let hydra_family = sketch(A::HydraCms);
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

/// A summary state's row holds its state's index, a key per text column
/// (`job` and the series identity), its window's time and the batch's
/// row slot beside the state, and the batch holds the texts once; the
/// state's own bytes, which a merge is costed by, leave them out. 1,000
/// per-series Sums carrying `job`: 116,000 bytes, 80,000 of them texts,
/// 28 per row beside each 8-byte record.
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
    assert_eq!(state_row_bytes_beside_state(&schema), 28);
    assert_eq!(text_dictionary_bytes(&schema, 1_000), 80_000);
    assert_eq!(state_rows_bytes(&schema, 1_000, 8), Some(116_000));
    let edge = EdgeStatistics {
        rows: 1_000,
        bytes: 116_000,
    };
    assert_eq!(state_bytes_per_row(edge, &schema), 8);
}
