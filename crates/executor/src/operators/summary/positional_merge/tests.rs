//! The merge by position against the hash merge's answer: which windows
//! merge by position, and, over random windows of every exact family and
//! of each sketch, the same merged states as a fold of each group's states
//! in window order (counters in time order), the hash merge's order.
use super::*;
use crate::plan::PhysicalDAG;
use crate::runtime::{Limits, Scope};
use crate::summary_kernels::exact::ExactAccumulator;
use crate::summary_kernels::weighted_frequency::WeightedFrequency;
use crate::summary_kernels::{
    DDSketchAccumulator, DatasketchesKLLAccumulator, HllSketchAccumulator, KmvAccumulator,
};
use crate::AggregateCore;
use futures::executor::block_on;
use planner_types::stages::Lifetime;
use planner_types::summary_type::{ExactKind, ExactParams, SketchAlgorithm};
use std::collections::BTreeMap;

use WindowMerge::{ByGroupTable, ByPosition};

fn plain_field(name: &str, dtype: DataType) -> SummaryField {
    SummaryField {
        name: name.into(),
        dtype: SummaryFamilyType::Plain(dtype),
        nullable: false,
    }
}

/// Raw samples: 0 `job` and 1 `series` (Utf8 groups), 2 `ts`, 3 `value`,
/// 4 `item` (the keyed sketches' item).
fn samples_schema() -> SchemaRef {
    Arc::new(Schema {
        fields: vec![
            plain_field("job", DataType::Utf8),
            plain_field("series", DataType::Utf8),
            plain_field("ts", DataType::Timestamp),
            plain_field("value", DataType::Float64),
            plain_field("item", DataType::Utf8),
        ],
        time_index: Some(2),
        unique_keys: vec![],
        closed: false,
    })
}

/// One raw sample: job, series, timestamp, value, item.
type Sample = (String, String, i64, f64, String);

fn context() -> RunContext {
    RunContext::new(
        Scope::Query {
            evaluation_time_ms: 1_000_000,
        },
        Limits::default(),
    )
    .unwrap()
}

/// Run `operator` over `inputs` from one source: its output batches.
fn run(operator: Operator, schema: SchemaRef, inputs: Vec<Batch>) -> Vec<Batch> {
    let mut dag = PhysicalDAG::default();
    dag.add(
        0,
        vec![],
        Operator::source(schema, inputs).unwrap(),
        Lifetime::None,
    )
    .unwrap();
    dag.add(1, vec![0], operator, Lifetime::None).unwrap();
    block_on(async {
        let mut output = dag.execute(&[1], context()).unwrap().remove(0);
        let mut batches = Vec::new();
        while let Some(batch) = output.next().await {
            batches.push((*batch.unwrap()).clone());
        }
        batches
    })
}

/// A family and its θ (`None` for an exact family).
type Family = (SummaryFamilyType, Option<SketchParams>);

fn exact(kind: ExactKind, params: ExactParams) -> Family {
    (SummaryFamilyType::ExactAggregate(kind, params), None)
}

fn sketch(algorithm: SketchAlgorithm, params: SketchParams) -> Family {
    (SummaryFamilyType::Sketch(algorithm), Some(params))
}

fn is_counter(family: &Family) -> bool {
    matches!(
        family.0,
        SummaryFamilyType::ExactAggregate(
            ExactKind::Rate | ExactKind::Increase | ExactKind::IRate,
            _
        )
    )
}

/// One window's states of `family` per `groups` (column indices of the
/// samples schema), as a chain stores them: the groups' labels in key
/// order, then the state, each state marked as held by the store.
fn window(family: &Family, groups: &[usize], samples: &[Sample]) -> Batch {
    let schema = samples_schema();
    let rows = samples
        .iter()
        .map(|(job, series, ts, value, item)| {
            vec![
                Value::Utf8(job.as_str().into()),
                Value::Utf8(series.as_str().into()),
                Value::Timestamp(*ts),
                Value::Float64(*value),
                Value::Utf8(item.as_str().into()),
            ]
        })
        .collect();
    let input = Batch::try_new(schema.clone(), rows).unwrap();
    let build = if matches!(
        family.0,
        SummaryFamilyType::Sketch(SketchAlgorithm::CmsWithHeap)
    ) {
        Operator::per_group_keyed_summary_build(
            schema.clone(),
            family.0.clone(),
            family.1.clone(),
            vec![4],
            RowWeight::Constant(NonZeroU32::new(1).unwrap()),
            groups.to_vec(),
        )
    } else {
        Operator::per_group_summary_build(
            schema.clone(),
            family.0.clone(),
            family.1.clone(),
            3,
            Some(2),
            groups.to_vec(),
        )
    }
    .unwrap();
    let mut built = run(build, schema, vec![input]);
    assert_eq!(built.len(), 1, "one window batch");
    let batch = built.remove(0);
    batch.mark_states_held_outside_run();
    batch
}

/// A deterministic random source (64-bit LCG).
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 11
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// A random value for `family`: for an exact sum, count, min or max, also
/// NaN, ±Inf, -0.0 and values whose sum loses precision; positive values
/// for a sketch.
fn random_value(random: &mut Random, family: &Family) -> f64 {
    match &family.0 {
        SummaryFamilyType::ExactAggregate(..) => match random.below(12) {
            0 => f64::NAN,
            1 => f64::INFINITY,
            2 => f64::NEG_INFINITY,
            3 => -0.0,
            4 => 1e300,
            5 => 1e-300,
            6 => -1e16,
            _ => (random.below(2_000) as f64 - 1_000.0) / 7.0,
        },
        _ => 1.0 + random.below(100_000) as f64 / 100.0,
    }
}

/// `count` windows of random samples of up to 12 series over 3 jobs, each
/// window `(k s, (k + 1) s]`: a series is missing from a window, or new
/// in it, now and then; a window is now and then empty; counter series
/// (with `counter`) only grow, but reset now and then.
fn random_samples(random: &mut Random, family: &Family, count: usize) -> Vec<Vec<Sample>> {
    let counter = is_counter(family);
    let mut levels = BTreeMap::<usize, f64>::new();
    let base: Vec<usize> = (0..12).filter(|_| random.chance(70)).collect();
    (0..count)
        .map(|k| {
            if random.chance(10) {
                return vec![];
            }
            let mut samples = Vec::new();
            for series in 0..12usize {
                let present = if base.contains(&series) {
                    !random.chance(15)
                } else {
                    random.chance(10)
                };
                if !present {
                    continue;
                }
                let per_window = 1 + random.below(4) as i64;
                for i in 0..per_window {
                    let ts = k as i64 * 1_000 + 1 + i * (999 / per_window);
                    let value = if counter {
                        let level = levels.entry(series).or_insert(0.0);
                        *level = if random.chance(15) {
                            random.below(5) as f64
                        } else {
                            *level + random.below(50) as f64
                        };
                        *level
                    } else {
                        random_value(random, family)
                    };
                    samples.push((
                        format!("j{}", series % 3),
                        format!("s{series:02}"),
                        ts,
                        value,
                        format!("i{}", random.below(9)),
                    ));
                }
            }
            samples
        })
        .collect()
}

/// The full content of a state, for a bit-for-bit comparison.
fn fingerprint(state: &dyn AggregateCore) -> String {
    let any = state.as_any();
    if let Some(s) = any.downcast_ref::<ExactAccumulator>() {
        // MessagePack keeps each f64's bits, NaN payload and -0.0 included.
        return format!("{:?}", rmp_serde::to_vec(s).unwrap());
    }
    if let Some(s) = any.downcast_ref::<DatasketchesKLLAccumulator>() {
        // `k` above any group's sample count: nothing compacts, so the
        // random compaction coin never flips.
        return format!(
            "KLL n={} q={:?}",
            s.inner.count(),
            [0.0, 0.1, 0.5, 0.9, 1.0].map(|q| s.inner.quantile(q).to_bits())
        );
    }
    if let Some(s) = any.downcast_ref::<DDSketchAccumulator>() {
        return format!("{s:?}");
    }
    if let Some(s) = any.downcast_ref::<HllSketchAccumulator>() {
        return format!("{s:?}");
    }
    if let Some(s) = any.downcast_ref::<KmvAccumulator>() {
        return format!("{s:?}");
    }
    if let Some(s) = any.downcast_ref::<WeightedFrequency>() {
        return format!("{:?} {:?}", s.counters(), s.rows(usize::MAX));
    }
    panic!("no fingerprint for this state")
}

/// A merged batch's rows: each row's group labels and its state's
/// fingerprint, in row order.
fn merged_rows(batch: &Batch, groups: usize) -> Vec<(Vec<String>, String)> {
    batch
        .to_rows()
        .into_iter()
        .map(|row| {
            let labels = row[..groups]
                .iter()
                .map(|label| match label {
                    Value::Utf8(text) => text.to_string(),
                    other => panic!("a text label, not {other:?}"),
                })
                .collect();
            let Value::Summary { state, .. } = &row[groups] else {
                panic!("a state column")
            };
            (labels, fingerprint(state.as_ref()))
        })
        .collect()
}

/// The reference answer: each group's states folded with the kernel's own
/// `merge_with`, in window order then row order, counter states in the
/// order of their first sample (a stable sort, as the hash merge sorts);
/// the groups in key order (their labels, ascending).
fn reference_merge(windows: &[Batch], groups: usize, counter: bool) -> Vec<(Vec<String>, String)> {
    let mut entries: Vec<(Vec<String>, Arc<dyn AggregateCore>)> = windows
        .iter()
        .flat_map(|window| {
            window.to_rows().into_iter().map(|row| {
                let labels = row[..groups]
                    .iter()
                    .map(|label| match label {
                        Value::Utf8(text) => text.to_string(),
                        other => panic!("a text label, not {other:?}"),
                    })
                    .collect::<Vec<_>>();
                let Value::Summary { state, .. } = &row[groups] else {
                    panic!("a state column")
                };
                (labels, state.clone())
            })
        })
        .collect();
    if counter {
        entries.sort_by_key(|(_, state)| {
            state
                .as_any()
                .downcast_ref::<ExactAccumulator>()
                .and_then(ExactAccumulator::earliest_sample_ms)
        });
    }
    let mut merged = BTreeMap::<Vec<String>, Box<dyn AggregateCore>>::new();
    for (labels, state) in entries {
        let next = match merged.remove(&labels) {
            None => state.clone_boxed_core(),
            Some(existing) => existing.merge_with(state.as_ref()).unwrap(),
        };
        merged.insert(labels, next);
    }
    merged
        .into_iter()
        .map(|(labels, state)| (labels, fingerprint(state.as_ref())))
        .collect()
}

/// The windows' merge by position, as a batch.
fn merged_by_position(windows: &[Batch], groups: &[usize]) -> Batch {
    let input_schema = windows[0].schema().clone();
    let merge = Operator::summary_merge(
        input_schema.clone(),
        groups.len(),
        (0..groups.len()).collect(),
    )
    .unwrap();
    let refs = windows.iter().collect::<Vec<_>>();
    block_on(merge_by_position(
        &refs,
        groups.len(),
        &(0..groups.len()).collect::<Vec<_>>(),
        &input_schema,
        &merge.output,
        &context(),
    ))
    .unwrap()
}

/// The hash merge (today's `SummaryMerge`) of the windows, as one batch.
fn merged_by_operator(windows: &[Batch], groups: usize) -> Vec<(Vec<String>, String)> {
    let schema = windows[0].schema().clone();
    let merge = Operator::summary_merge(schema.clone(), groups, (0..groups).collect()).unwrap();
    run(merge, schema, windows.to_vec())
        .iter()
        .flat_map(|batch| merged_rows(batch, groups))
        .collect()
}

fn exact_families() -> Vec<Family> {
    vec![
        exact(ExactKind::Sum, ExactParams::Sum),
        exact(ExactKind::Count, ExactParams::Count),
        exact(ExactKind::Min, ExactParams::Min),
        exact(ExactKind::Max, ExactParams::Max),
        exact(ExactKind::Rate, ExactParams::Rate),
        exact(ExactKind::Increase, ExactParams::Increase),
    ]
}

fn sketch_families() -> Vec<Family> {
    vec![
        sketch(
            SketchAlgorithm::DDSketch,
            SketchParams::DDSketch { alpha: 0.01 },
        ),
        sketch(SketchAlgorithm::Kll, SketchParams::Kll { k: 1024 }),
        sketch(SketchAlgorithm::Hll, SketchParams::Hll { precision: 10 }),
        sketch(SketchAlgorithm::Kmv, SketchParams::Kmv { k: 32 }),
        sketch(
            SketchAlgorithm::CmsWithHeap,
            SketchParams::CmsWithHeap {
                width: 64,
                depth: 3,
                heap_size: 8,
            },
        ),
    ]
}

/// Windows of `series` (one sample each, value 1) per `series` group.
fn windows_of(series: &[&[&str]]) -> Vec<Batch> {
    let family = exact(ExactKind::Sum, ExactParams::Sum);
    series
        .iter()
        .enumerate()
        .map(|(k, names)| {
            let samples = names
                .iter()
                .map(|name| {
                    (
                        "j".to_string(),
                        name.to_string(),
                        k as i64 * 1_000 + 1,
                        1.0,
                        "i".to_string(),
                    )
                })
                .collect::<Vec<_>>();
            window(&family, &[1], &samples)
        })
        .collect()
}

// Consecutive windows of the same series list them in the same key order:
// every window merges by position, though each window's text labels are
// encoded with its own dictionary.
#[test]
fn windows_of_the_same_series_merge_by_position() {
    let windows = windows_of(&[&["a", "b", "c"], &["a", "b", "c"], &["a", "b", "c"]]);
    let refs = windows.iter().collect::<Vec<_>>();
    assert_eq!(
        plan_window_merges(&refs, &[0]).unwrap(),
        vec![ByPosition, ByPosition, ByPosition]
    );
}

// A window in which a series ended, or appeared, lists other rows than the
// first window: it merges through the group table; the others still merge
// by position.
#[test]
fn a_window_whose_series_differ_merges_through_the_group_table() {
    let windows = windows_of(&[
        &["a", "b", "c"],
        &["a", "c"],
        &["a", "b", "c"],
        &["a", "b", "c", "d"],
    ]);
    let refs = windows.iter().collect::<Vec<_>>();
    assert_eq!(
        plan_window_merges(&refs, &[0]).unwrap(),
        vec![ByPosition, ByGroupTable, ByPosition, ByGroupTable]
    );
}

// Rows in another order are other rows: a window whose rows a store
// returned in another order merges through the group table.
#[test]
fn a_window_in_another_row_order_merges_through_the_group_table() {
    let windows = windows_of(&[&["a", "b", "c"], &["a", "b", "c"]]);
    let reversed = windows[1].take(&UInt32Array::from(vec![2, 1, 0])).unwrap();
    let refs = vec![&windows[0], &reversed];
    assert_eq!(
        plan_window_merges(&refs, &[0]).unwrap(),
        vec![ByPosition, ByGroupTable]
    );
}

// The same dictionary keys over other text are other rows: window 0
// encodes `[a, b]` and window 1 `[a, z]` both as keys `[0, 1]`, so a
// comparison of keys alone would merge `z` into `b`.
#[test]
fn equal_dictionary_keys_over_other_text_are_other_rows() {
    let windows = windows_of(&[&["a", "b"], &["a", "z"]]);
    let refs = windows.iter().collect::<Vec<_>>();
    assert_eq!(
        plan_window_merges(&refs, &[0]).unwrap(),
        vec![ByPosition, ByGroupTable]
    );
    let merged = merged_by_position(&windows, &[0]);
    assert_eq!(merged_rows(&merged, 1), reference_merge(&windows, 1, false));
}

// A first window that repeats a group (series states rolled up to their
// job) cannot merge by position: every window merges through the group
// table, and the answer is still the hash merge's.
#[test]
fn a_first_window_that_repeats_a_group_merges_every_window_through_the_group_table() {
    let windows = windows_of(&[&["a", "b"], &["a", "b"]]);
    // Group by `job` (column 0 of the projected rows): both rows are job `j`.
    let by_job = windows
        .iter()
        .map(|window| {
            let rows = window
                .to_rows()
                .into_iter()
                .map(|row| vec![Value::Utf8("j".into()), row[1].clone()])
                .collect();
            let schema = Arc::new(Schema {
                fields: vec![
                    plain_field("job", DataType::Utf8),
                    window.schema().fields[1].clone(),
                ],
                time_index: None,
                unique_keys: vec![],
                closed: false,
            });
            Batch::try_new(schema, rows).unwrap()
        })
        .collect::<Vec<_>>();
    let refs = by_job.iter().collect::<Vec<_>>();
    assert_eq!(
        plan_window_merges(&refs, &[0]).unwrap(),
        vec![ByGroupTable, ByGroupTable]
    );
    assert_eq!(
        merged_rows(&merged_by_position(&by_job, &[0]), 1),
        reference_merge(&by_job, 1, false)
    );
}

// Two group columns: a window that differs only in the second one is
// other rows.
#[test]
fn every_group_column_is_compared() {
    let family = exact(ExactKind::Sum, ExactParams::Sum);
    let sample = |job: &str, series: &str, k: i64| {
        (
            job.to_string(),
            series.to_string(),
            k * 1_000 + 1,
            1.0,
            "i".to_string(),
        )
    };
    let windows = [
        window(
            &family,
            &[0, 1],
            &[sample("j0", "a", 0), sample("j1", "b", 0)],
        ),
        window(
            &family,
            &[0, 1],
            &[sample("j0", "a", 1), sample("j1", "b", 1)],
        ),
        window(
            &family,
            &[0, 1],
            &[sample("j0", "a", 2), sample("j1", "c", 2)],
        ),
    ];
    let refs = windows.iter().collect::<Vec<_>>();
    assert_eq!(
        plan_window_merges(&refs, &[0, 1]).unwrap(),
        vec![ByPosition, ByPosition, ByGroupTable]
    );
}

// With no group column, windows of one row each merge by position.
#[test]
fn windows_of_one_row_and_no_group_column_merge_by_position() {
    let family = exact(ExactKind::Sum, ExactParams::Sum);
    let windows = (0..3)
        .map(|k| {
            window(
                &family,
                &[],
                &[("j".into(), "a".into(), k * 1_000 + 1, 2.0, "i".into())],
            )
        })
        .collect::<Vec<_>>();
    let refs = windows.iter().collect::<Vec<_>>();
    assert_eq!(
        plan_window_merges(&refs, &[]).unwrap(),
        vec![ByPosition, ByPosition, ByPosition]
    );
}

/// Over `cases` random inputs of `family`, `answer` gives each case's
/// merged rows; each must equal the reference fold.
fn check_random_windows(
    family: &Family,
    cases: u64,
    answer: impl Fn(&[Batch], usize) -> Vec<(Vec<String>, String)>,
) {
    for case in 0..cases {
        let mut random = Random(case * 7919 + 17);
        let count = 1 + random.below(8) as usize;
        let samples = random_samples(&mut random, family, count);
        let Some(any) = samples.iter().find(|s| !s.is_empty()) else {
            continue;
        };
        // An empty window: no rows, in the windows' schema.
        let empty = window(family, &[1], any).slice(0, 0);
        let mut windows = samples
            .iter()
            .map(|samples| {
                if samples.is_empty() {
                    empty.clone()
                } else {
                    window(family, &[1], samples)
                }
            })
            .collect::<Vec<_>>();
        // A store may return windows out of time order; counter states
        // must still merge in time order.
        if is_counter(family) && random.chance(20) {
            windows.reverse();
        }
        let expected = reference_merge(&windows, 1, is_counter(family));
        assert_eq!(
            answer(&windows, 1),
            expected,
            "{:?}, case {case}: {} windows",
            family.0,
            windows.len()
        );
    }
}

// The reference fold is the hash merge's answer, bit for bit: the same
// states, in the same row order. This passes before the merge by position
// exists and keeps the reference honest after it.
#[test]
fn the_reference_fold_is_the_hash_merge() {
    for family in exact_families().iter().chain(&sketch_families()) {
        check_random_windows(family, 60, merged_by_operator);
    }
}

// Every exact family, over random windows with series churn, empty
// windows, NaN, ±Inf, -0.0, cancelling sums and counter resets (and
// windows out of time order for counters): the merge by position returns
// the hash merge's states bit for bit, in the same row order.
#[test]
fn exact_states_merge_by_position_as_the_hash_merge_does() {
    for family in exact_families() {
        check_random_windows(&family, 200, |windows, groups| {
            merged_rows(
                &merged_by_position(windows, &(0..groups).collect::<Vec<_>>()),
                groups,
            )
        });
    }
}

// DDSketch, KLL (below its first compaction), HLL, KMV and Count-Min with
// heap, over random windows with series churn and empty windows: the merge
// by position returns the hash merge's sketches, the same in every
// estimate.
#[test]
fn sketches_merge_by_position_as_the_hash_merge_does() {
    for family in sketch_families() {
        check_random_windows(&family, 60, |windows, groups| {
            merged_rows(
                &merged_by_position(windows, &(0..groups).collect::<Vec<_>>()),
                groups,
            )
        });
    }
}
