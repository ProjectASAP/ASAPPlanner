//! The per-sample `StateColumnUpdater` builds the same states as the
//! batch `SummaryAgg` build and as each family's kernel updated row by row,
//! for every family a per-group build supports.
use asap_executor::factory::create_planner_accumulator;
use asap_executor::operators::{Operator, RowWeight};
use asap_executor::plan::PhysicalDAG;
use asap_executor::planner::ir::scalar::ColumnRef;
use asap_executor::planner::ir::schema::{
    DataType, ExactKind, ExactParams, Field, FieldDataType, GroupingStrategy, HydraKind,
    HydraParams, Schema, SketchAlgorithm, SketchKind, SketchParams, SketchStatistic, SummaryUpdate,
};
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::summary_kernels::weighted_frequency::WeightedFrequency;
use asap_executor::summary_kernels::{
    exact::ExactAccumulator, univmon::UnivMonAccumulator, DDSketchAccumulator,
    DatasketchesKLLAccumulator, HllSketchAccumulator, KmvAccumulator,
};
use asap_executor::values::{Batch, SchemaRef, Value};
use asap_executor::{AggregateCore, StateColumnUpdater};
use futures::{executor::block_on, StreamExt};
use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::Arc;

fn plain(name: &str, dtype: DataType) -> Field {
    Field {
        name: name.into(),
        dtype: FieldDataType::Plain(dtype),
        nullable: false,
        table: None,
    }
}

/// Columns: 0 `series` (Utf8 group), 1 `ts` (Timestamp), 2 `value`
/// (Float64), 3 `item` (Utf8).
fn input_schema() -> SchemaRef {
    Arc::new(Schema {
        fields: vec![
            plain("series", DataType::Utf8),
            plain("ts", DataType::Timestamp),
            plain("value", DataType::Float64),
            plain("item", DataType::Utf8),
        ],
        time_index: Some(1),
        unique_keys: vec![],
        closed: false,
    })
}

/// 600 samples of 7 series, in time order per series; values are positive
/// counters with resets, so every family takes them.
fn samples() -> Vec<(String, i64, f64, String)> {
    let mut seed = 11u64;
    let mut counters = [0.0f64; 7];
    (0..600)
        .map(|i| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let series = (seed >> 40) as usize % 7;
            counters[series] = if (seed >> 20).is_multiple_of(50) {
                1.0
            } else {
                counters[series] + 1.0 + ((seed >> 30) % 9) as f64
            };
            (
                format!("s{series}"),
                1_000 + i as i64 * 10,
                counters[series],
                format!("i{}", (seed >> 50) % 13),
            )
        })
        .collect()
}

fn input_batch() -> Batch {
    Batch::try_new(
        input_schema(),
        samples()
            .into_iter()
            .map(|(series, ts, value, item)| {
                vec![
                    Value::Utf8(series.into()),
                    Value::Timestamp(ts),
                    Value::Float64(value),
                    Value::Utf8(item.into()),
                ]
            })
            .collect(),
    )
    .unwrap()
}

fn sketch(algorithm: SketchAlgorithm, params: SketchParams) -> FieldDataType {
    FieldDataType::Sketch(SketchKind::new(algorithm, params), Default::default())
}

fn exact(kind: ExactKind, params: ExactParams) -> FieldDataType {
    FieldDataType::ExactAggregate(kind, params)
}

/// Every family a per-group value build supports.
fn value_families() -> Vec<FieldDataType> {
    vec![
        exact(ExactKind::Sum, ExactParams::Sum),
        exact(ExactKind::Count, ExactParams::Count),
        exact(ExactKind::Min, ExactParams::Min),
        exact(ExactKind::Max, ExactParams::Max),
        exact(ExactKind::Rate, ExactParams::Rate),
        exact(ExactKind::Increase, ExactParams::Increase),
        sketch(
            SketchAlgorithm::Kll,
            SketchParams::Kll {
                // Above any group's sample count (at most 600): no
                // compaction, so the random compaction coin never flips.
                k: 1024,
                failure_probability: None,
            },
        ),
        sketch(
            SketchAlgorithm::DDSketch,
            SketchParams::DDSketch { alpha: 0.01 },
        ),
        sketch(
            SketchAlgorithm::Hll,
            SketchParams::Hll {
                precision: 10,
                relative_error: None,
            },
        ),
        sketch(
            SketchAlgorithm::Kmv,
            SketchParams::Kmv {
                k: 32,
                relative_error: None,
            },
        ),
        sketch(
            SketchAlgorithm::UnivMon,
            SketchParams::UnivMon {
                heap_size: 64,
                sketch_rows: 5,
                sketch_cols: 128,
                layers: 4,
                distinct_failure_probability: None,
            },
        ),
    ]
}

/// Every keyed family a per-group keyed build supports.
fn keyed_families() -> Vec<FieldDataType> {
    vec![
        sketch(
            SketchAlgorithm::CmsWithHeap,
            SketchParams::CmsWithHeap {
                width: 64,
                depth: 3,
                heap_size: 8,
            },
        ),
        sketch(
            SketchAlgorithm::CountSketchWithHeap,
            SketchParams::CountSketchWithHeap {
                width: 64,
                depth: 3,
                heap_size: 8,
            },
        ),
    ]
}

/// The full content of a state, for byte-for-byte comparison (a KLL's count
/// and quantiles, a UnivMon's answers).
fn fingerprint(state: &dyn AggregateCore) -> String {
    let any = state.as_any();
    if let Some(s) = any.downcast_ref::<ExactAccumulator>() {
        return serde_json::to_string(s).unwrap();
    }
    if let Some(s) = any.downcast_ref::<DatasketchesKLLAccumulator>() {
        // asap_sketchlib seeds an unseeded KLL's compaction coin from the
        // wall clock; with `k` above the sample count nothing compacts, so
        // its count and quantiles are deterministic.
        return format!(
            "KLL k={} n={} q={:?}",
            s.inner.k(),
            s.inner.count(),
            [0.1, 0.5, 0.9, 1.0].map(|q| s.inner.quantile(q).to_bits())
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
    if let Some(s) = any.downcast_ref::<UnivMonAccumulator>() {
        // Its heaps' internal order depends on hash-map iteration, so even
        // two merges of the same states differ in which heavy hitters they
        // keep, and so in their distinct count: compare its other answers.
        use asap_executor::planner::ir::schema::SketchStatistic as Q;
        let count = Q::PointCount {
            key: ColumnRef::SampleValue,
            value: None,
        };
        return [count, Q::FrequencyL2, Q::FrequencyEntropy]
            .iter()
            .map(|query| s.estimate(query).unwrap().to_bits().to_string())
            .collect::<Vec<_>>()
            .join(",");
    }
    if let Some(s) = any.downcast_ref::<WeightedFrequency>() {
        return format!("{:?} {:?}", s.counters(), s.rows(usize::MAX));
    }
    if let Some(s) = any.downcast_ref::<asap_executor::summary_kernels::HydraCmsGroup>() {
        // The group's name and its count of every item the samples use.
        let counts = (0..13)
            .map(|item| {
                let query = SketchStatistic::PointCount {
                    key: ColumnRef::Named("item".into()),
                    value: Some(format!("i{item}")),
                };
                s.estimate(&query).unwrap().to_string()
            })
            .collect::<Vec<_>>();
        return format!("{} {}", s.group, counts.join(","));
    }
    panic!("no fingerprint for this state")
}

/// Run `build` over the input batch: each group's state, by series.
fn batch_build(build: Operator) -> BTreeMap<String, String> {
    let mut dag = PhysicalDAG::default();
    dag.add(
        0,
        vec![],
        Operator::source(input_schema(), vec![input_batch()]).unwrap(),
    )
    .unwrap();
    dag.add(1, vec![0], build).unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 10_000,
            revision: 1,
        },
        Limits::default(),
    )
    .unwrap();
    let output = block_on(async {
        dag.execute(&[1], context)
            .unwrap()
            .remove(0)
            .next()
            .await
            .unwrap()
            .unwrap()
    });
    let states = output.state_column(1).unwrap();
    let indices = output
        .column(1)
        .as_any()
        .downcast_ref::<arrow::array::UInt32Array>()
        .unwrap();
    output
        .to_rows()
        .iter()
        .enumerate()
        .map(|(row, cells)| {
            let Value::Utf8(series) = &cells[0] else {
                panic!()
            };
            (
                series.to_string(),
                fingerprint(states.state(indices.value(row) as usize).as_ref()),
            )
        })
        .collect()
}

/// One test sample: series, timestamp, value, item.
type TestSample = (String, i64, f64, String);
/// How a test feeds one sample to an updater.
type FeedSample<'a> = dyn FnMut(&mut StateColumnUpdater, u32, &TestSample) + 'a;

/// The per-sample build: one updater, groups numbered as series appear.
/// With `sharded`, the first half of the samples go to one updater and the
/// rest to another, merged at the end as a runtime's shards are.
fn per_sample_build(
    family: &FieldDataType,
    sharded: bool,
    mut update: impl FnMut(&mut StateColumnUpdater, u32, &(String, i64, f64, String)),
) -> BTreeMap<String, String> {
    let samples = samples();
    let mut groups: BTreeMap<String, u32> = BTreeMap::new();
    for sample in &samples {
        let next = groups.len() as u32;
        groups.entry(sample.0.clone()).or_insert(next);
    }
    let shard = |part: &[(String, i64, f64, String)], update: &mut FeedSample<'_>| {
        let mut updater = StateColumnUpdater::new(family).unwrap();
        updater.ensure_groups(groups.len()).unwrap();
        for sample in part {
            update(&mut updater, groups[&sample.0], sample);
        }
        updater
    };
    let split = if sharded {
        samples.len() / 2
    } else {
        samples.len()
    };
    let (first, second) = samples.split_at(split);
    let mut updater = shard(first, &mut update);
    if sharded {
        updater.merge(shard(second, &mut update)).unwrap();
    }
    assert_eq!(updater.group_count(), groups.len());
    let states = updater.seal();
    groups
        .into_iter()
        .map(|(series, group)| (series, fingerprint(states.state(group as usize).as_ref())))
        .collect()
}

/// The pre-columnar reference: each family's kernel, one per series,
/// updated row by row. With `sharded`, two halves merged through the
/// kernel's own merge.
fn kernel_build(family: &FieldDataType, keyed: bool, sharded: bool) -> BTreeMap<String, String> {
    let update = if keyed {
        SummaryUpdate {
            item: Some(
                asap_executor::planner::ir::schema::SummaryInputExpr::Column(ColumnRef::Named(
                    "item".into(),
                )),
            ),
            weight: asap_executor::planner::ir::schema::SummaryInputExpr::Constant(1.0),
            weight_domain: Default::default(),
        }
    } else {
        SummaryUpdate::column(ColumnRef::SampleValue)
    };
    let samples = samples();
    let split = if sharded {
        samples.len() / 2
    } else {
        samples.len()
    };
    let (first, second) = samples.split_at(split);
    let half = |part: &[(String, i64, f64, String)]| {
        let mut states = BTreeMap::new();
        for (series, ts, value, item) in part {
            let state = states.entry(series.clone()).or_insert_with(|| {
                if keyed {
                    let FieldDataType::Sketch(kind, _) = family else {
                        unreachable!()
                    };
                    let (algorithm, width, depth, capacity) = match kind.params() {
                        SketchParams::CmsWithHeap {
                            width,
                            depth,
                            heap_size,
                        } => (
                            asap_executor::summary_kernels::weighted_frequency::FrequencyAlgorithm::Cms,
                            width,
                            depth,
                            heap_size,
                        ),
                        SketchParams::CountSketchWithHeap {
                            width,
                            depth,
                            heap_size,
                        } => (
                            asap_executor::summary_kernels::weighted_frequency::FrequencyAlgorithm::CountSketch,
                            width,
                            depth,
                            heap_size,
                        ),
                        _ => unreachable!(),
                    };
                    Err(Box::new(
                        WeightedFrequency::new(
                            algorithm,
                            *width as usize,
                            *depth as usize,
                            *capacity as usize,
                        )
                        .unwrap(),
                    ))
                } else {
                    Ok(create_planner_accumulator(family, &update, &Default::default()).unwrap())
                }
            });
            match state {
                Ok(updater) => updater.update_value(&Value::Float64(*value), *ts).unwrap(),
                Err(keyed) => keyed
                    .update(&[Value::Utf8(item.as_str().into())], 1.0)
                    .unwrap(),
            }
        }
        states
            .into_iter()
            .map(|(series, state)| {
                (
                    series,
                    match state {
                        Ok(updater) => Arc::<dyn AggregateCore>::from(updater.into_accumulator()),
                        Err(keyed) => Arc::new(*keyed) as Arc<dyn AggregateCore>,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>()
    };
    let (mut merged, second) = (half(first), half(second));
    for (series, state) in second {
        let combined = match merged.get(&series) {
            Some(existing) => Arc::from(existing.merge_with(state.as_ref()).unwrap()),
            None => state,
        };
        merged.insert(series, combined);
    }
    merged
        .into_iter()
        .map(|(series, state)| (series, fingerprint(state.as_ref())))
        .collect()
}

// For every value family: the per-sample updater, the batch `SummaryAgg`
// build and the family's own kernel build the same state for every group,
// byte for byte; two updater shards merged equal the kernel's merge of the
// same two halves.
#[test]
fn per_sample_updates_build_the_batch_states_for_every_value_family() {
    for family in value_families() {
        let batch = batch_build(
            Operator::per_group_summary_build(input_schema(), family.clone(), 2, Some(1), vec![0])
                .unwrap(),
        );
        let update =
            |updater: &mut StateColumnUpdater, group, sample: &(String, i64, f64, String)| {
                updater.update(group, sample.2, sample.1).unwrap()
            };
        assert_eq!(batch.len(), 7, "{family:?}");
        assert_eq!(
            per_sample_build(&family, false, update),
            batch,
            "{family:?}: updater vs batch build"
        );
        assert_eq!(
            kernel_build(&family, false, false),
            batch,
            "{family:?}: kernel vs batch build"
        );
        assert_eq!(
            per_sample_build(&family, true, update),
            kernel_build(&family, false, true),
            "{family:?}: merged shards vs kernel merge"
        );
    }
}

// The same for the keyed families (an item and a weight per sample).
#[test]
fn per_sample_updates_build_the_batch_states_for_every_keyed_family() {
    for family in keyed_families() {
        let batch = batch_build(
            Operator::per_group_keyed_summary_build(
                input_schema(),
                family.clone(),
                vec![3],
                RowWeight::Constant(NonZeroU32::new(1).unwrap()),
                vec![0],
            )
            .unwrap(),
        );
        let update =
            |updater: &mut StateColumnUpdater, group, sample: &(String, i64, f64, String)| {
                updater
                    .update_item(group, &[Value::Utf8(sample.3.as_str().into())], 1.0)
                    .unwrap()
            };
        assert_eq!(batch.len(), 7, "{family:?}");
        assert_eq!(
            per_sample_build(&family, false, update),
            batch,
            "{family:?}: updater vs batch build"
        );
        assert_eq!(
            kernel_build(&family, true, false),
            batch,
            "{family:?}: kernel vs batch build"
        );
        assert_eq!(
            per_sample_build(&family, true, update),
            kernel_build(&family, true, true),
            "{family:?}: merged shards vs kernel merge"
        );
    }
}

// A counter refuses a sample that is not after its group's previous one,
// and a value outside the kernel's domain, leaving the state unchanged.
#[test]
fn per_sample_updates_keep_each_kernels_input_contract() {
    let mut rate = StateColumnUpdater::new(&exact(ExactKind::Rate, ExactParams::Rate)).unwrap();
    let group = rate.add_group().unwrap();
    rate.update(group, 1.0, 100).unwrap();
    assert!(rate.update(group, 2.0, 100).is_err());
    assert!(rate.update(group, f64::INFINITY, 200).is_err());
    rate.update(group, 3.0, 200).unwrap();
    let mut dd = StateColumnUpdater::new(&sketch(
        SketchAlgorithm::DDSketch,
        SketchParams::DDSketch { alpha: 0.01 },
    ))
    .unwrap();
    let group = dd.add_group().unwrap();
    assert!(dd.update(group, -1.0, 0).is_err());
    // A keyed family takes items, a value family takes values.
    let mut cms = StateColumnUpdater::new(&keyed_families()[0]).unwrap();
    let group = cms.add_group().unwrap();
    assert!(cms.update(group, 1.0, 0).is_err());
    assert!(dd
        .update_item(group, &[Value::Utf8("a".into())], 1.0)
        .is_err());
}

// Counter shards must not overlap in time: their increases would be added
// twice. Shard A has samples at t=10 and t=30, shard B at t=20 and t=40;
// their merge is refused, not (v30-v10)+(v40-v20).
#[test]
fn merging_counter_shards_that_overlap_in_time_is_refused() {
    for family in [
        exact(ExactKind::Rate, ExactParams::Rate),
        exact(ExactKind::Increase, ExactParams::Increase),
    ] {
        let shard = |samples: &[(i64, f64)]| {
            let mut updater = StateColumnUpdater::new(&family).unwrap();
            let group = updater.add_group().unwrap();
            for &(ts, value) in samples {
                updater.update(group, value, ts).unwrap();
            }
            updater
        };
        let mut a = shard(&[(10, 1.0), (30, 3.0)]);
        let b = shard(&[(20, 2.0), (40, 4.0)]);
        assert!(
            matches!(a.merge(b), Err(asap_executor::Error::Invalid(_))),
            "{family:?}"
        );
        // Disjoint shards merge, in either order.
        let mut late = shard(&[(30, 3.0), (40, 4.0)]);
        late.merge(shard(&[(10, 1.0), (20, 2.0)])).unwrap();
        let mut early = shard(&[(10, 1.0), (20, 2.0)]);
        early.merge(shard(&[(30, 3.0), (40, 4.0)])).unwrap();
        let read = |updater: StateColumnUpdater| {
            serde_json::to_string(
                updater
                    .seal()
                    .state(0)
                    .as_any()
                    .downcast_ref::<ExactAccumulator>()
                    .unwrap(),
            )
            .unwrap()
        };
        assert_eq!(read(late), read(early));
    }
}

// A group number the updater never added is bad input, not a panic.
#[test]
fn an_unknown_group_is_bad_input() {
    let mut sum = StateColumnUpdater::new(&exact(ExactKind::Sum, ExactParams::Sum)).unwrap();
    sum.add_group().unwrap();
    assert!(matches!(
        sum.update(1, 1.0, 0),
        Err(asap_executor::Error::Invalid(_))
    ));
    let mut cms = StateColumnUpdater::new(&keyed_families()[0]).unwrap();
    assert!(matches!(
        cms.update_item(0, &[Value::Utf8("a".into())], 1.0),
        Err(asap_executor::Error::Invalid(_))
    ));
    sum.ensure_groups(3).unwrap();
    assert_eq!(sum.group_count(), 3);
    sum.update(2, 1.0, 0).unwrap();
}

/// A HydraCms family: one shared grid for all groups.
fn hydra_family() -> FieldDataType {
    FieldDataType::Sketch(
        SketchKind::new(
            SketchAlgorithm::Cms,
            SketchParams::Cms {
                width: 64,
                depth: 3,
            },
        ),
        GroupingStrategy::SharedMultiSubpopulation {
            kind: HydraKind::HydraCms,
            params: HydraParams::HydraCms {
                width: 64,
                depth: 3,
                shared_rows: 3,
                shared_columns: 16,
            },
        },
    )
}

/// The per-sample HydraCms build: groups added with their key as series
/// appear, one count per sample; with `sharded`, two halves merged.
fn hydra_per_sample_build(sharded: bool) -> BTreeMap<String, String> {
    let family = hydra_family();
    let samples = samples();
    let shard = |part: &[TestSample]| {
        let mut updater = StateColumnUpdater::new(&family).unwrap();
        let mut groups = BTreeMap::<String, u32>::new();
        for sample in part {
            let group = match groups.get(&sample.0) {
                Some(&group) => group,
                None => {
                    let key = [Value::Utf8(sample.0.as_str().into())];
                    let group = updater.add_group_with_key(&key).unwrap();
                    groups.insert(sample.0.clone(), group);
                    group
                }
            };
            updater
                .update_item(group, &[Value::Utf8(sample.3.as_str().into())], 1.0)
                .unwrap();
        }
        (updater, groups)
    };
    let (first, second) = samples.split_at(if sharded { 300 } else { 600 });
    let (mut updater, mut groups) = shard(first);
    if sharded {
        // The second shard numbers groups as the first does: the runtime's
        // shared map from labels to group numbers.
        let mut other = StateColumnUpdater::new(&family).unwrap();
        let mut by_number = groups
            .iter()
            .map(|(series, &group)| (group, series.clone()))
            .collect::<BTreeMap<_, _>>();
        for sample in second {
            if !groups.contains_key(&sample.0) {
                let group = groups.len() as u32;
                groups.insert(sample.0.clone(), group);
                by_number.insert(group, sample.0.clone());
            }
        }
        for series in by_number.values() {
            other
                .add_group_with_key(&[Value::Utf8(series.as_str().into())])
                .unwrap();
        }
        for sample in second {
            other
                .update_item(
                    groups[&sample.0],
                    &[Value::Utf8(sample.3.as_str().into())],
                    1.0,
                )
                .unwrap();
        }
        updater.merge(other).unwrap();
    }
    assert_eq!(updater.group_count(), groups.len());
    let states = updater.seal();
    groups
        .into_iter()
        .map(|(series, group)| (series, fingerprint(states.state(group as usize).as_ref())))
        .collect()
}

// HydraCms: the per-sample updater builds the batch build's shared grid,
// group for group (same subpopulation names, same item counts), and two
// shards merged build it too.
#[test]
fn per_sample_updates_build_the_batch_hydra_grid() {
    let batch = batch_build(
        Operator::all_groups_summary_build(
            input_schema(),
            hydra_family(),
            3,
            RowWeight::Constant(NonZeroU32::new(1).unwrap()),
            vec![0],
        )
        .unwrap(),
    );
    assert_eq!(batch.len(), 7);
    assert_eq!(
        hydra_per_sample_build(false),
        batch,
        "updater vs batch build"
    );
    assert_eq!(
        hydra_per_sample_build(true),
        batch,
        "merged shards vs batch build"
    );
}

// A HydraCms group needs its key (its subpopulation name), takes items and
// integer weights only, and shards that name a group differently are bad
// input.
#[test]
fn hydra_updates_keep_the_kernel_input_contract() {
    let family = hydra_family();
    let mut updater = StateColumnUpdater::new(&family).unwrap();
    assert!(matches!(
        updater.add_group(),
        Err(asap_executor::Error::Invalid(_))
    ));
    let group = updater
        .add_group_with_key(&[Value::Utf8("api".into())])
        .unwrap();
    assert!(updater.update(group, 1.0, 0).is_err());
    assert!(updater
        .update_item(group, &[Value::Utf8("a".into())], 1.5)
        .is_err());
    assert!(updater
        .update_item(group, &[Value::Utf8("a".into())], -1.0)
        .is_err());
    updater
        .update_item(group, &[Value::Utf8("a".into())], 2.0)
        .unwrap();
    let before = updater.memory_bytes();
    assert!(before > 0);
    // Every key has the first key's arity and, per position, one value
    // type (NULL allowed anywhere).
    let bad_key = |updater: &mut StateColumnUpdater, key: &[Value]| {
        matches!(
            updater.add_group_with_key(key),
            Err(asap_executor::Error::Invalid(_))
        )
    };
    assert!(bad_key(&mut updater, &[]));
    assert!(bad_key(
        &mut updater,
        &[Value::Utf8("web".into()), Value::Int64(1)]
    ));
    assert!(bad_key(&mut updater, &[Value::Int64(1)]));
    assert_eq!(updater.group_count(), 1);
    updater.add_group_with_key(&[Value::Null]).unwrap();
    assert_eq!(updater.group_count(), 2);
    let mut other = StateColumnUpdater::new(&family).unwrap();
    other
        .add_group_with_key(&[Value::Utf8("web".into())])
        .unwrap();
    assert!(matches!(
        updater.merge(other),
        Err(asap_executor::Error::Invalid(_))
    ));
}
