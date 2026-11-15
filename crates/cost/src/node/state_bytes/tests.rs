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
            summary_shape(
                &FieldDataType::ExactAggregate(kind, params),
                None,
                StateInputRows::UNKNOWN
            ),
            Ok((1.0, 56))
        );
    }
    assert_eq!(asap_types::summary_type::EXACT_ACCUMULATOR_STATE_BYTES, 56);
}

/// The rows a family is fed.
enum Rows {
    /// One sample value a row.
    Values,
    /// One label value a row, as Pass 1 feeds UnivMon a column.
    Labels,
    /// A label set and a weight a row.
    Keyed,
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
    // Ten Pareto(2) samples (scale 1), one 1 s tumbling window of a 10 Hz series.
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
        sketch_shape(
            &SketchParams::Kll { k },
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
            let input = match rows {
                Rows::Values => &value,
                Rows::Labels => &labeled,
                Rows::Keyed => &keyed,
            };
            let mut updater = create_planner_accumulator(&family, Some(params), input).unwrap();
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

/// How far plan selection's bytes for a state may be from what the
/// executor charges for it in [`priced_bytes_agree_with_the_executor`]:
/// priced ÷ charged in `[1 / 1.5, 1.5]`.
const BYTES_AGREEMENT_FACTOR: f64 = 1.5;

/// The families whose priced bytes are known to disagree with the
/// executor's beyond [`BYTES_AGREEMENT_FACTOR`]: (family, the issue that
/// fixes it, priced ÷ charged as measured by
/// [`priced_bytes_agree_with_the_executor`]). A family leaves the list
/// when its issue is fixed: the test fails while it is listed but agrees,
/// or when its ratio moves off the one listed.
const KNOWN_BYTES_GAPS: &[(&str, u32, f64)] = &[
    // Priced for values over nine decades (`DDSKETCH_ASSUMED_VALUE_RANGE`);
    // these span three.
    ("DDSketch", 1313, 2.982),
    // Priced as one element of the executor's state column (56 bytes);
    // a boxed accumulator charges its whole struct (136 bytes).
    ("exact Sum accumulator", 1301, 0.412),
    ("exact Count accumulator", 1301, 0.412),
    ("exact Min accumulator", 1301, 0.412),
    ("exact Max accumulator", 1301, 0.412),
    ("exact Increase accumulator", 1301, 0.412),
    ("exact Rate accumulator", 1301, 0.412),
    // Priced as one Count-Min per group; the executor keeps one grid for
    // all of them, here 3 × 64 cells over 10 groups.
    ("HydraCms", 947, 0.104),
];

/// The next value of the seeded stream `state` (splitmix64), uniform in
/// `[0, 1)`, so the rows every family is fed are fixed.
fn next_unit(state: &mut u64) -> f64 {
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
/// built through the executor and fed 10,000 seeded rows: latencies
/// log-uniform over 1 ms to 1 s (declared non-negative), or 1,000
/// distinct label values or label sets. Its charge
/// (`approx_memory_bytes`; for HydraCms its shared grid over its 10
/// groups) is compared with `summary_shape` for the same rows. Priced ÷
/// charged lies within [`BYTES_AGREEMENT_FACTOR`], or the family is in
/// [`KNOWN_BYTES_GAPS`] with its issue and measured ratio.
///
/// HydraKll is left out: no plan builds it (it has no parameter list,
/// and the executor's factory refuses it).
#[test]
fn priced_bytes_agree_with_the_executor() {
    use asap_executor::factory::{create_hydra_cms, create_planner_accumulator};
    use asap_executor::values::Value;
    use asap_types::ir::scalar::expr_ir::ColumnRef;
    use asap_types::summary_type::{
        ExactKind, ExactParams, HydraParams, SummaryInputExpr, SummaryUpdate,
    };
    use SketchAlgorithm as A;
    const ROWS: u32 = 10_000;
    const DISTINCT_KEYS: u32 = 1_000;
    const HYDRA_GROUPS: u32 = 10;
    let input_rows = |rows_per_state: u32| StateInputRows {
        rows_per_state: Some(u64::from(rows_per_state)),
        non_negative: true,
    };
    let sketch = FieldDataType::Sketch;
    let exact = |kind, params| {
        (
            FieldDataType::ExactAggregate(kind, params),
            None,
            Rows::Values,
        )
    };
    // (family, its parameters, the rows it is fed).
    let families = [
        (
            sketch(A::Kll),
            Some(SketchParams::Kll { k: 269 }),
            Rows::Values,
        ),
        (
            sketch(A::DDSketch),
            Some(SketchParams::DDSketch { alpha: 0.01 }),
            Rows::Values,
        ),
        (
            sketch(A::Hll),
            Some(SketchParams::Hll { precision: 18 }),
            Rows::Values,
        ),
        (
            sketch(A::Kmv),
            Some(SketchParams::Kmv { k: 106_263 }),
            Rows::Values,
        ),
        (
            sketch(A::Cms),
            Some(SketchParams::Cms {
                width: 272,
                depth: 5,
            }),
            Rows::Keyed,
        ),
        (
            sketch(A::CountSketch),
            Some(SketchParams::CountSketch {
                width: 30_000,
                depth: 47,
            }),
            Rows::Keyed,
        ),
        (
            sketch(A::CmsWithHeap),
            Some(SketchParams::CmsWithHeap {
                width: 272,
                depth: 5,
                heap_size: 10,
            }),
            Rows::Keyed,
        ),
        (
            sketch(A::CountSketchWithHeap),
            Some(SketchParams::CountSketchWithHeap {
                width: 30_000,
                depth: 47,
                heap_size: 10,
            }),
            Rows::Keyed,
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
        ),
        exact(ExactKind::Sum, ExactParams::Sum),
        exact(ExactKind::Count, ExactParams::Count),
        exact(ExactKind::Min, ExactParams::Min),
        exact(ExactKind::Max, ExactParams::Max),
        exact(ExactKind::Increase, ExactParams::Increase),
        exact(ExactKind::Rate, ExactParams::Rate),
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
    // (family name, priced bytes, charged bytes).
    let mut measured: Vec<(String, u64, u64)> = Vec::new();
    for (family, params, rows) in &families {
        let input = match rows {
            Rows::Values => &value,
            Rows::Labels => &labeled,
            Rows::Keyed => &keyed,
        };
        let mut updater = create_planner_accumulator(family, params.as_ref(), input).unwrap();
        let mut seed = 0x5EED_u64;
        for i in 0..ROWS {
            let key = (next_unit(&mut seed) * f64::from(DISTINCT_KEYS)) as u32;
            match rows {
                // One sample a second, its latency log-uniform over 1 ms to 1 s.
                Rows::Values => {
                    let latency_ms = 1_000f64.powf(next_unit(&mut seed));
                    updater
                        .update_single(latency_ms, i64::from(i) * 1_000)
                        .unwrap();
                }
                Rows::Labels => updater
                    .update_value(&Value::Utf8(label(key).into()), 0)
                    .unwrap(),
                Rows::Keyed => updater.update_keyed(&labels(key), 1.0, 0).unwrap(),
            }
        }
        let charged = updater.into_accumulator().approx_memory_bytes() as u64;
        let priced = summary_shape(family, params.as_ref(), input_rows(ROWS))
            .unwrap()
            .1;
        let name = crate::node::usage::summary_type_name(family);
        measured.push((name, priced, charged));
    }
    // HydraCms: one grid for every group, priced as one Count-Min per group.
    let hydra = SketchParams::Hydra(HydraParams::HydraCms {
        width: 272,
        depth: 5,
        shared_rows: 3,
        shared_columns: 64,
    });
    let hydra_family = sketch(A::HydraCms);
    let mut grid = create_hydra_cms(&hydra_family, Some(&hydra)).unwrap();
    let mut seed = 0x5EED_u64;
    for _ in 0..ROWS {
        let group = (next_unit(&mut seed) * f64::from(HYDRA_GROUPS)) as u32;
        let item = (next_unit(&mut seed) * f64::from(DISTINCT_KEYS)) as u32;
        grid.count(&label(group), &label(item), 1).unwrap();
    }
    measured.push((
        crate::node::usage::summary_type_name(&hydra_family),
        sketch_shape(&hydra, input_rows(ROWS / HYDRA_GROUPS)).1,
        (grid.approx_memory_bytes() / HYDRA_GROUPS as usize) as u64,
    ));

    let mut failures = Vec::new();
    for (name, priced, charged) in &measured {
        let ratio = *priced as f64 / *charged as f64;
        let agrees = (1.0 / BYTES_AGREEMENT_FACTOR..=BYTES_AGREEMENT_FACTOR).contains(&ratio);
        let line = format!("{name}: {priced} priced, {charged} charged, ratio {ratio:.3}");
        match KNOWN_BYTES_GAPS.iter().find(|(gap, ..)| gap == name) {
            Some((_, issue, _)) if agrees => failures.push(format!(
                "{line}: agrees now; remove it from KNOWN_BYTES_GAPS (#{issue})"
            )),
            Some((_, issue, listed)) if (ratio / listed - 1.0).abs() > 0.05 => failures.push(
                format!("{line}: listed at {listed} in KNOWN_BYTES_GAPS (#{issue}); update it"),
            ),
            Some(_) => {}
            None if !agrees => failures.push(format!(
                "{line}: outside {BYTES_AGREEMENT_FACTOR}x; fix it or list it in KNOWN_BYTES_GAPS with its issue"
            )),
            None => {}
        }
    }
    for (gap, issue, _) in KNOWN_BYTES_GAPS {
        if !measured.iter().any(|(name, ..)| name == gap) {
            failures.push(format!(
                "KNOWN_BYTES_GAPS lists {gap} (#{issue}), which is not measured"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
