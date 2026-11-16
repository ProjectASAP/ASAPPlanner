use super::*;
use asap_types::summary_type::SketchAlgorithm;

/// An exact accumulator's state is priced at the executor's per-group
/// state, 56 bytes for every kind (a counter's state alone is 48), so a
/// tumbling-window chain of exact sums keeps 56 bytes per tumbling window and group.
#[test]
fn exact_state_is_priced_at_the_executors_state_size() {
    use asap_types::summary_type::{ExactKind, ExactParams};
    for (kind, params) in [
        (ExactKind::Sum, ExactParams::Sum),
        (ExactKind::Increase, ExactParams::Increase),
        (ExactKind::Rate, ExactParams::Rate),
    ] {
        assert_eq!(
            summary_state_bytes(
                &FieldDataType::ExactAggregate(kind, params),
                None,
                StateInputRows::UNKNOWN
            ),
            Ok(56)
        );
    }
    assert_eq!(asap_types::summary_type::EXACT_ACCUMULATOR_STATE_BYTES, 56);
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

/// A KLL state is charged what `asap_sketchlib` allocates for the
/// samples it sees, not its full capacity: its fixed struct and level
/// array, plus the item array the library starts empty and doubles from
/// 16 items as samples arrive, up to `compute_max_capacity(k, 8)`. E.g.
/// a 10 s tumbling window of a series sampled every 15 s sees one sample: 744 + 128
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
        sketch_state_bytes(
            &SketchParams::Kll { k },
            StateInputRows {
                rows_per_state,
                non_negative: true,
            },
        )
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
/// pricing assumes: signed values over [`DDSKETCH_ASSUMED_VALUE_RANGE`]
/// (signed latency-like values for DDSketch), label values of [`ASSUMED_VALUE_KEY_BYTES`] keys for UnivMon, or
/// keyed rows whose tuple keys are [`ASSUMED_HEAP_KEY_BYTES`] long. The
/// heap families' shapes are dominated by their heaps. The charge is
/// the sketch's bytes plus the accumulator's own fields, named per
/// family. It equals the priced bytes where `asap_sketchlib`'s estimate
/// is exact (Count-Min, Count Sketch, HLL, KMV at `n` distinct items) and
/// for KLL up to `k` rows. DDSketch is priced at what it is expected to
/// hold, within 1.5 times either way. For the heap sketches and UnivMon
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
    use asap_types::summary_type::{SummaryInputExpr, SummaryUpdate};
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
        /// Within 1.5 times either way.
        Expected,
    }
    /// The rows a family is fed.
    enum Rows {
        /// Signed values over the assumed range.
        Values,
        /// Signed latency-like values ([`latency_like`]).
        LatencyLike,
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
    let value = SummaryUpdate::column(ColumnRef::SampleValue);
    let item = Some(SummaryInputExpr::Column(ColumnRef::Named("item".into())));
    let keyed = SummaryUpdate {
        item: item.clone(),
        ..value.clone()
    };
    let labeled = SummaryUpdate {
        item,
        weight: SummaryInputExpr::Constant(1.0),
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
            let family = FieldDataType::Sketch(*algorithm);
            let input = match rows {
                Rows::Values | Rows::LatencyLike => &value,
                Rows::Labels => &labeled,
                Rows::Keyed => &keyed,
            };
            let mut updater = create_planner_accumulator(&family, Some(params), input).unwrap();
            let latency = latency_like(0, n);
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
            let state = updater.into_accumulator();
            let charged = state.approx_memory_bytes();
            let priced = fields
                + sketch_state_bytes(
                    params,
                    StateInputRows {
                        rows_per_state: Some(u64::from(n)),
                        non_negative: false,
                    },
                ) as usize;
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

/// A summary state's row holds its labels, its window's time and its
/// series identity beside the state; the state's own bytes, which a merge
/// is costed by, leave them out. A stored per-series Sum carrying `job`:
/// 144 bytes per row, 56 of them its state.
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
    assert_eq!(state_row_bytes_beside_state(&schema), 88);
    let edge = EdgeStatistics {
        rows: 1_000,
        bytes: 144_000,
    };
    assert_eq!(state_bytes_per_row(edge, &schema), 56);
}
