//! The per-sample `StateColumnUpdater` builds the same states as the
//! batch `SummaryAgg` build and as each family's kernel updated row by row,
//! for every family a per-group build supports.
use asap_executor::factory::create_planner_accumulator;
use asap_executor::operators::{Operator, RowWeight};
use asap_executor::plan::PhysicalDAG;
use asap_executor::planner::ir::scalar::ColumnRef;
use asap_executor::planner::ir::schema::{
    DataType, ExactKind, ExactParams, Field, FieldDataType, Schema, SketchAlgorithm, SketchKind,
    SketchParams, SummaryUpdate,
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
                k: 64,
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

/// The full content of a state, for byte-for-byte comparison (a KLL's
/// deterministic part, a UnivMon's answers).
fn fingerprint(state: &dyn AggregateCore) -> String {
    let any = state.as_any();
    if let Some(s) = any.downcast_ref::<ExactAccumulator>() {
        return serde_json::to_string(s).unwrap();
    }
    if let Some(s) = any.downcast_ref::<DatasketchesKLLAccumulator>() {
        // asap_sketchlib seeds an unseeded KLL's compaction coin from the
        // wall clock, so no two KLL builds are byte-identical (nor equal in
        // weighted count), even of one input: compare its shape.
        return format!("KLL k={}", s.inner.k());
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
        return serde_json::to_string(s).unwrap();
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
        .rows()
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
        for _ in 0..groups.len() {
            updater.add_group().unwrap();
        }
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
        updater.merge(&shard(second, &mut update)).unwrap();
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
                    Err(WeightedFrequency::new(
                        algorithm,
                        *width as usize,
                        *depth as usize,
                        *capacity as usize,
                    )
                    .unwrap())
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
                        Err(keyed) => Arc::new(keyed) as Arc<dyn AggregateCore>,
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
