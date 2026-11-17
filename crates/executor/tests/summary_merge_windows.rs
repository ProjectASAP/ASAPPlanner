//! A `SummaryMerge` of a chain's stored windows: each sketch kernel merges
//! in place into the state `merge_with` returns; sketches merged across an
//! hour of windows stay within their error bounds; and (ignored by
//! default, run with `--release -- --ignored`) the time per merged window,
//! against the targets of the merge by position.
use asap_executor::planner::ir::schema::FieldDataType;
use asap_executor::planner::summary_type::{
    ExactKind, ExactParams, SketchAlgorithm, SketchParams, SketchStatistic,
};
use asap_executor::summary_kernels::weighted_frequency::WeightedFrequency;
use asap_executor::values::{Batch, Value};
use asap_executor::AggregateCore;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use crate::common::state_fingerprint::fingerprint;
use crate::common::stored_windows::{
    merge, sample_item, sample_value, series_label, stored_windows, Family, WindowShape,
};

fn exact(kind: ExactKind, params: ExactParams) -> Family {
    (FieldDataType::ExactAggregate(kind, params), None)
}

fn sketch(algorithm: SketchAlgorithm, params: SketchParams) -> Family {
    (FieldDataType::Sketch(algorithm), Some(params))
}

fn ddsketch() -> Family {
    sketch(
        SketchAlgorithm::DDSketch,
        SketchParams::DDSketch { alpha: 0.01 },
    )
}

fn kll(k: u32) -> Family {
    sketch(SketchAlgorithm::Kll, SketchParams::Kll { k })
}

fn cms_with_heap() -> Family {
    sketch(
        SketchAlgorithm::CmsWithHeap,
        SketchParams::CmsWithHeap {
            width: 272,
            depth: 5,
            heap_size: 16,
        },
    )
}

/// Each row's series label and state.
fn states(batch: &Batch) -> Vec<(String, Arc<dyn AggregateCore>)> {
    batch
        .to_rows()
        .into_iter()
        .map(|row| {
            let Value::Utf8(series) = &row[0] else {
                panic!("a series label")
            };
            let Value::Summary { state, .. } = &row[1] else {
                panic!("a state")
            };
            (series.to_string(), state.clone())
        })
        .collect()
}

// For every exact family and every sketch: a state merged in place with
// another becomes the state `merge_with` returns, bit for bit; merging
// states of different families is an error, not a panic.
#[test]
fn merge_in_place_leaves_the_state_merge_with_returns() {
    let shape = WindowShape {
        series: 3,
        windows: 2,
        samples_per_window: 50,
    };
    let families = [
        exact(ExactKind::Sum, ExactParams::Sum),
        exact(ExactKind::Count, ExactParams::Count),
        exact(ExactKind::Min, ExactParams::Min),
        exact(ExactKind::Max, ExactParams::Max),
        exact(ExactKind::Rate, ExactParams::Rate),
        exact(ExactKind::Increase, ExactParams::Increase),
        ddsketch(),
        // `k` above the sample count: nothing compacts, so the random
        // compaction coin never flips.
        kll(1024),
        sketch(SketchAlgorithm::Hll, SketchParams::Hll { precision: 10 }),
        sketch(SketchAlgorithm::Kmv, SketchParams::Kmv { k: 32 }),
        cms_with_heap(),
    ];
    for family in &families {
        let windows = stored_windows(family, shape);
        let (first, second) = (states(&windows[0]), states(&windows[1]));
        for ((series, left), (_, right)) in first.iter().zip(&second) {
            let expected = left.merge_with(right.as_ref()).unwrap();
            let mut in_place = left.clone_boxed_core();
            in_place.merge_in_place(right.as_ref()).unwrap();
            assert_eq!(
                fingerprint(in_place.as_ref()),
                fingerprint(expected.as_ref()),
                "{:?}, series {series}",
                family.0
            );
        }
    }
    let shape = WindowShape {
        windows: 1,
        ..shape
    };
    let sum = states(&stored_windows(&families[0], shape)[0]).remove(0).1;
    let dd = states(&stored_windows(&ddsketch(), shape)[0]).remove(0).1;
    assert!(dd.clone_boxed_core().merge_in_place(sum.as_ref()).is_err());
    assert!(sum.clone_boxed_core().merge_in_place(dd.as_ref()).is_err());
}

/// The interpolated quantile `q` of `values` (PromQL's
/// `quantile_over_time`): between the order statistics at ranks
/// ⌊q (n − 1)⌋ and ⌈q (n − 1)⌉.
fn interpolated_quantile(sorted: &[f64], q: f64) -> f64 {
    let rank = q * (sorted.len() - 1) as f64;
    let (low, high) = (rank.floor() as usize, rank.ceil() as usize);
    sorted[low] + (rank - low as f64) * (sorted[high] - sorted[low])
}

// Sixty 1-minute windows of 50 samples per series, merged into one state
// per series: each sketch's estimates stay within its error bound against
// the exact answer over the 3,000 samples. DDSketch (α = 0.01): every
// quantile within 1% relative. KLL (k = 200): the rank of every quantile
// within 3% of q. HLL (precision 12, standard error 1.6%): the distinct
// count within 5%. KMV (k = 256, standard error 6.3%): within 20%.
// Count-Min (width 272, depth 5, ε = e / 272): each item's count at least
// its true count and at most 2 ε N above it.
#[test]
fn sketches_stay_within_their_error_bounds_after_sixty_merges() {
    let shape = WindowShape {
        series: 4,
        windows: 60,
        samples_per_window: 50,
    };
    let quantiles = [0.01, 0.25, 0.5, 0.75, 0.99];
    let values = |family: &Family, series: usize| {
        let mut values = (0..shape.windows)
            .flat_map(|window| {
                (0..shape.samples_per_window)
                    .map(move |sample| sample_value(&family.0, shape, series, window, sample))
            })
            .collect::<Vec<_>>();
        values.sort_by(f64::total_cmp);
        values
    };
    let merged = |family: &Family| states(&merge(&stored_windows(family, shape)));

    for (series, state) in merged(&ddsketch())
        .iter()
        .enumerate()
        .map(|(i, s)| (i, &s.1))
    {
        let values = values(&ddsketch(), series);
        for q in quantiles {
            let estimate = state.estimate(&SketchStatistic::Quantile { q }).unwrap();
            let exact = interpolated_quantile(&values, q);
            assert!(
                (estimate - exact).abs() <= 0.01 * exact * (1.0 + 1e-9),
                "DDSketch series {series} q {q}: {estimate} vs {exact}"
            );
        }
    }
    for (series, (_, state)) in merged(&kll(200)).iter().enumerate() {
        let values = values(&kll(200), series);
        let n = values.len() as f64;
        for q in quantiles {
            let estimate = state.estimate(&SketchStatistic::Quantile { q }).unwrap();
            let rank = values.partition_point(|v| *v <= estimate) as f64 / n;
            assert!(
                (rank - q).abs() <= 0.03,
                "KLL series {series} q {q}: rank {rank} of {estimate}"
            );
        }
    }
    for (family, bound) in [
        (
            sketch(SketchAlgorithm::Hll, SketchParams::Hll { precision: 12 }),
            0.05,
        ),
        (
            sketch(SketchAlgorithm::Kmv, SketchParams::Kmv { k: 256 }),
            0.20,
        ),
    ] {
        for (series, (_, state)) in merged(&family).iter().enumerate() {
            let mut distinct = values(&family, series);
            distinct.dedup();
            let exact = distinct.len() as f64;
            let estimate = state.estimate(&SketchStatistic::Cardinality).unwrap();
            assert!(
                (estimate - exact).abs() <= bound * exact,
                "{:?} series {series}: {estimate} vs {exact}",
                family.0
            );
        }
    }
    let samples = (shape.windows * shape.samples_per_window) as f64;
    let slack = 2.0 * std::f64::consts::E / 272.0 * samples;
    for (series, (label, state)) in merged(&cms_with_heap()).iter().enumerate() {
        assert_eq!(label, &series_label(series));
        let mut counts = BTreeMap::<String, f64>::new();
        for window in 0..shape.windows {
            for sample in 0..shape.samples_per_window {
                *counts
                    .entry(sample_item(series, window, sample))
                    .or_default() += 1.0;
            }
        }
        let frequency = state.as_any().downcast_ref::<WeightedFrequency>().unwrap();
        for (item, count) in counts {
            let estimate = frequency
                .point_estimate(&[Value::Utf8(item.as_str().into())])
                .unwrap();
            assert!(
                count <= estimate && estimate <= count + slack,
                "Count-Min series {series} item {item}: {estimate} vs {count}"
            );
        }
    }
}

/// The median over 5 runs of the time a `SummaryMerge` of `windows` takes,
/// per merged window of one series (one input state).
fn ns_per_merged_window(windows: &[Batch]) -> f64 {
    let states = windows.iter().map(Batch::num_rows).sum::<usize>() as f64;
    merge(windows); // Warm up.
    let mut runs = (0..5)
        .map(|_| {
            let start = Instant::now();
            std::hint::black_box(merge(windows));
            start.elapsed().as_nanos() as f64 / states
        })
        .collect::<Vec<_>>();
    runs.sort_by(f64::total_cmp);
    runs[2]
}

/// Measure the merge of `family`'s windows in `shape`, print the time per
/// merged window, and check it against `target_ns`.
fn check_merge_time(name: &str, family: &Family, shape: WindowShape, target_ns: f64) {
    if cfg!(debug_assertions) {
        panic!("run the merge benchmarks with --release");
    }
    let windows = stored_windows(family, shape);
    let ns = ns_per_merged_window(&windows);
    println!(
        "merge benchmark: {name}: {ns:.1} ns per merged window \
         ({} series x {} windows, {} samples per window; target {target_ns} ns)",
        shape.series, shape.windows, shape.samples_per_window
    );
    assert!(
        ns <= target_ns,
        "{name}: {ns:.1} ns per merged window, target {target_ns} ns"
    );
}

/// `sum_over_time(m[1h])` read every 60 s over 1,000 series: 60 windows.
const SERIES_HOUR: WindowShape = WindowShape {
    series: 1_000,
    windows: 60,
    samples_per_window: 20,
};

// An exact sum merges one window of one series in at most 30 ns (today
// about 125 ns: a batch copy, a hash lookup of the series identity and a
// row-at-a-time merge per state).
#[test]
#[ignore = "wall-clock benchmark: cargo test --release -- --ignored"]
fn benchmark_sum_merge_per_window() {
    check_merge_time(
        "Sum",
        &exact(ExactKind::Sum, ExactParams::Sum),
        SERIES_HOUR,
        30.0,
    );
}

// A counter (rate) merges one window of one series in at most 60 ns, its
// resets included (today about 135 ns, with a sort by first sample).
#[test]
#[ignore = "wall-clock benchmark: cargo test --release -- --ignored"]
fn benchmark_rate_merge_per_window() {
    check_merge_time(
        "Rate",
        &exact(ExactKind::Rate, ExactParams::Rate),
        SERIES_HOUR,
        60.0,
    );
}

// A DDSketch (α = 0.01) of 200 samples merges in place in at most 400 ns
// per window: about one add per bucket of the input state, with no copy of
// the running state and no new allocation (today a copy and a new box per
// merge, about 1.8 µs).
#[test]
#[ignore = "wall-clock benchmark: cargo test --release -- --ignored"]
fn benchmark_ddsketch_merge_per_window() {
    check_merge_time(
        "DDSketch",
        &ddsketch(),
        WindowShape {
            series: 200,
            windows: 60,
            samples_per_window: 200,
        },
        400.0,
    );
}

// A KLL (k = 200) of 200 samples merges in at most 10 µs per window, no
// slower than today (about 8.8 µs): its time is the merge's own
// compaction, which the merge by position leaves to the sketch library;
// only the copy of the running state goes.
#[test]
#[ignore = "wall-clock benchmark: cargo test --release -- --ignored"]
fn benchmark_kll_merge_per_window() {
    check_merge_time(
        "KLL",
        &kll(200),
        WindowShape {
            series: 200,
            windows: 60,
            samples_per_window: 200,
        },
        10_000.0,
    );
}
