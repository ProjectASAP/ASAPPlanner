//! A runtime that persists states and reads them back in place, through the
//! executor's public API only: it takes each state's view bytes from a state
//! column, lays them out in a buffer it owns (here a `Vec<u64>`, as a mapped
//! file would be), and builds a state column over that buffer again. The
//! rebuilt column answers and merges as the original, and reads each state
//! where the buffer holds it.
use asap_executor::arrow::array::{DictionaryArray, UInt32Array};
use asap_executor::arrow::buffer::{Buffer, ScalarBuffer};
use asap_executor::arrow::datatypes::UInt32Type;
use asap_executor::operators::Operator;
use asap_executor::plan::PhysicalDAG;
use asap_executor::planner::ir::schema::DataType;
use asap_executor::planner::ir::schema::{Field, FieldDataType, Schema};
use asap_executor::planner::stages::Lifetime;
use asap_executor::planner::summary_type::{
    ExactKind, ExactParams, SketchAlgorithm, SketchParams, SketchStatistic,
};
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::summary_kernels::exact::ExactAccumulator;
use asap_executor::summary_kernels::CountMinSketchAccumulator;
use asap_executor::values::{
    family_has_view_layout, Batch, BatchColumn, SchemaRef, StateColumn, Value,
};
use asap_executor::{AggregateCore, StateColumnUpdater};
use futures::{executor::block_on, StreamExt};
use std::borrow::Cow;
use std::sync::Arc;

const GROUPS: u32 = 5;

/// A sealed column of `GROUPS` states of `family`, group `g` fed
/// `40 + 10 g` samples of a counter that grows by 1 to 9 per sample.
fn sealed(family: &FieldDataType, params: Option<SketchParams>) -> StateColumn {
    let mut updater = StateColumnUpdater::new(family, params.as_ref()).unwrap();
    for group in 0..GROUPS {
        updater.add_group().unwrap();
        let mut value = 0.0;
        for sample in 0..40 + 10 * group as i64 {
            value += 1.0 + ((sample * 7 + group as i64) % 9) as f64;
            updater.update(group, value, 1_000 + sample * 10).unwrap();
        }
    }
    updater.seal()
}

/// A column of `GROUPS` Count-Min states built as objects (a keyless
/// Count-Min has no per-sample updater), through literal rows.
fn count_min_column() -> Arc<StateColumn> {
    let family = FieldDataType::Sketch(SketchAlgorithm::CountMinSketch);
    let schema = state_schema(&family);
    let rows = (0..GROUPS)
        .map(|group| {
            let mut cms = CountMinSketchAccumulator::new(3, 64).unwrap();
            for item in 0..20 + group {
                cms.update(&format!("i{}", item % 7), 1.0 + item as f64);
            }
            vec![Value::Summary {
                family: family.clone(),
                state: Arc::new(cms),
            }]
        })
        .collect();
    Batch::try_new(schema, rows)
        .unwrap()
        .state_column(0)
        .unwrap()
        .clone()
}

/// The view bytes of `column`'s states laid out as a runtime would store
/// them: back to back in one 8-byte-aligned buffer, each padded to 8 bytes,
/// and the `u32` offsets of their starts and the end.
fn stored(column: &StateColumn) -> (Buffer, ScalarBuffer<u32>) {
    let mut words: Vec<u64> = Vec::new();
    let mut offsets = vec![0u32];
    for index in 0..column.len() {
        let view = column.view_bytes(index).expect("a family with views");
        for chunk in view.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            words.push(u64::from_le_bytes(word));
        }
        offsets.push((words.len() * 8) as u32);
    }
    (Buffer::from_vec(words), ScalarBuffer::from(offsets))
}

/// `original` written as view bytes and read back over the stored buffer:
/// every view reads where the buffer holds it, and the column is held
/// outside the run, so a batch of it is charged no state memory.
fn round_trip(original: &StateColumn, family: &FieldDataType) -> StateColumn {
    assert!(family_has_view_layout(family), "{family:?}");
    let (bytes, offsets) = stored(original);
    let stored_range = bytes.as_ptr() as usize..bytes.as_ptr() as usize + bytes.len();
    let rebuilt = StateColumn::from_views(family.clone(), bytes.clone(), offsets).unwrap();
    assert_eq!(rebuilt.len(), original.len());
    for index in 0..rebuilt.len() {
        let Some(Cow::Borrowed(view)) = rebuilt.view_bytes(index) else {
            panic!("{family:?} state {index} is not read in place");
        };
        assert!(stored_range.contains(&(view.as_ptr() as usize)));
        assert!(stored_range.contains(&(view.as_ptr() as usize + view.len() - 1)));
        // The same view the original state wrote, up to the padding.
        let written = original.view_bytes(index).unwrap();
        assert_eq!(&view[..written.len()], &written[..]);
        assert!(view[written.len()..].iter().all(|&byte| byte == 0));
    }
    let schema = state_schema(family);
    let rebuilt = Arc::new(rebuilt);
    let batch = Batch::try_from_columns(
        schema,
        vec![BatchColumn::States {
            indices: UInt32Array::from_iter_values(0..rebuilt.len() as u32),
            states: rebuilt.clone(),
        }],
    )
    .unwrap();
    // Charged none of the states, whose stored size is what they read.
    assert!(rebuilt.state_memory_bytes() > 0);
    assert_eq!(
        batch.stored_bytes() - batch.memory_bytes(),
        rebuilt.state_memory_bytes()
    );
    drop(batch);
    Arc::into_inner(rebuilt).unwrap()
}

/// `query`'s answer on each state and on each adjacent pair merged, the same
/// on both columns.
fn assert_same_answers(original: &StateColumn, rebuilt: &StateColumn, query: &SketchStatistic) {
    for index in 0..original.len() {
        assert_eq!(
            original.state(index).estimate(query).unwrap(),
            rebuilt.state(index).estimate(query).unwrap(),
            "state {index}"
        );
    }
    for index in 1..original.len() {
        let merged = |column: &StateColumn| {
            column
                .state(index - 1)
                .merge_with(column.state(index).as_ref())
                .unwrap()
                .estimate(query)
                .unwrap()
        };
        assert_eq!(merged(original), merged(rebuilt), "merge {index}");
    }
}

#[test]
fn ddsketch_kll_and_count_min_states_read_back_in_place() {
    for (algorithm, params) in [
        (
            SketchAlgorithm::DDSketch,
            SketchParams::DDSketch { alpha: 0.01 },
        ),
        // `k` above every group's sample count: no compaction coin flips,
        // so merging owned states is reproducible.
        (SketchAlgorithm::KLL, SketchParams::Kll { k: 1024 }),
    ] {
        let family = FieldDataType::Sketch(algorithm);
        let original = sealed(&family, Some(params));
        let rebuilt = round_trip(&original, &family);
        for q in [0.1, 0.5, 0.99] {
            assert_same_answers(&original, &rebuilt, &SketchStatistic::Quantile { q });
        }
    }
    let family = FieldDataType::Sketch(SketchAlgorithm::CountMinSketch);
    let original = count_min_column();
    let rebuilt = round_trip(&original, &family);
    let total = SketchStatistic::PointCount {
        key: asap_executor::planner::ir::scalar::ColumnRef::SampleValue,
        value: None,
    };
    assert_same_answers(&original, &rebuilt, &total);
    for index in 0..original.len() {
        let point = |column: &StateColumn, item: &str| {
            let state = column.state(index);
            state
                .as_any()
                .downcast_ref::<CountMinSketchAccumulator>()
                .unwrap()
                .query_key(&[item.to_string()])
        };
        for item in ["i0", "i3", "i6", "absent"] {
            assert_eq!(point(&original, item), point(&rebuilt, item));
        }
    }
}

/// The schema of one state column of `family`.
fn state_schema(family: &FieldDataType) -> SchemaRef {
    Arc::new(Schema {
        fields: vec![Field {
            name: "state".into(),
            dtype: family.clone(),
            nullable: false,
        }],
        time_index: None,
        unique_keys: vec![],
        closed: false,
    })
}

/// An exact state as its JSON, equal for equal states.
fn exact_json(state: &dyn AggregateCore) -> serde_json::Value {
    serde_json::to_value(state.as_any().downcast_ref::<ExactAccumulator>().unwrap()).unwrap()
}

#[test]
fn exact_states_read_back_in_place() {
    let families = [
        (ExactKind::Sum, ExactParams::Sum),
        (ExactKind::Count, ExactParams::Count),
        (ExactKind::Min, ExactParams::Min),
        (ExactKind::Max, ExactParams::Max),
        (ExactKind::Counter, ExactParams::Counter),
        (ExactKind::IRate, ExactParams::IRate),
    ];
    for (kind, params) in families {
        let family = FieldDataType::ExactAggregate(kind, params);
        // Objects, as literal rows hold them: one state per group, the
        // last one empty.
        let rows = (0..GROUPS)
            .map(|group| {
                let mut state = ExactAccumulator::new(family.clone(), false).unwrap();
                for sample in 0..(GROUPS - 1 - group) as i64 * 3 {
                    state.update(None, 1.5 + (sample % 4) as f64, 1_000 + sample * 10);
                }
                vec![Value::Summary {
                    family: family.clone(),
                    state: Arc::new(state),
                }]
            })
            .collect();
        let objects = Batch::try_new(state_schema(&family), rows)
            .unwrap()
            .state_column(0)
            .unwrap()
            .clone();
        let mut originals = vec![objects];
        // Sealed exact records, for the families a per-sample build takes.
        if kind != ExactKind::IRate {
            originals.push(Arc::new(sealed(&family, None)));
        }
        for original in originals {
            let rebuilt = round_trip(&original, &family);
            for index in 0..original.len() {
                assert_eq!(
                    exact_json(original.state(index).as_ref()),
                    exact_json(rebuilt.state(index).as_ref()),
                    "{kind:?} state {index}"
                );
            }
        }
    }
}

/// `SummaryMerge` of `states` by `series`: each series' merged state, by
/// series. The merge reads a column built from views where it lies.
fn merged_by_series(
    family: &FieldDataType,
    series: &[&str],
    states: Arc<StateColumn>,
) -> Vec<Arc<dyn AggregateCore>> {
    let schema: SchemaRef = Arc::new(Schema {
        fields: vec![
            Field {
                name: "series".into(),
                dtype: FieldDataType::Plain(DataType::Utf8),
                nullable: false,
            },
            Field {
                name: "state".into(),
                dtype: family.clone(),
                nullable: false,
            },
        ],
        time_index: None,
        unique_keys: vec![],
        closed: false,
    });
    let labels: DictionaryArray<UInt32Type> = series.iter().copied().collect();
    let batch = Batch::try_from_columns(
        schema.clone(),
        vec![
            BatchColumn::Plain(Arc::new(labels)),
            BatchColumn::States {
                indices: UInt32Array::from_iter_values(0..states.len() as u32),
                states,
            },
        ],
    )
    .unwrap();
    let mut dag = PhysicalDAG::default();
    dag.add(
        0,
        vec![],
        Operator::source(schema.clone(), vec![batch]).unwrap(),
        Lifetime::None,
    )
    .unwrap();
    dag.add(
        1,
        vec![0],
        Operator::summary_merge(schema, 1, vec![0]).unwrap(),
        Lifetime::None,
    )
    .unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
        },
        Limits { max_bytes: 1 << 30 },
    )
    .unwrap();
    let mut rows: Vec<(String, Arc<dyn AggregateCore>)> = block_on(async {
        let mut output = dag.execute(&[1], context).unwrap().remove(0);
        let mut rows = Vec::new();
        while let Some(batch) = output.next().await {
            for row in batch.unwrap().to_rows() {
                let (Value::Utf8(series), Value::Summary { state, .. }) = (&row[0], &row[1]) else {
                    panic!("a series and its merged state");
                };
                rows.push((series.to_string(), state.clone()));
            }
        }
        rows
    });
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.into_iter().map(|(_, state)| state).collect()
}

/// A plan merging states read back in place answers as over the original
/// states: DDSketch and KLL quantiles, Count-Min totals, exact sums.
#[test]
fn a_merge_over_states_read_back_in_place_answers_as_over_the_originals() {
    let series = ["a", "b", "a", "b", "a"];
    let quantiles = [0.1, 0.5, 0.99].map(|q| SketchStatistic::Quantile { q });
    let total = [SketchStatistic::PointCount {
        key: asap_executor::planner::ir::scalar::ColumnRef::SampleValue,
        value: None,
    }];
    let sketches: [(FieldDataType, Arc<StateColumn>, &[SketchStatistic]); 3] = [
        (
            FieldDataType::Sketch(SketchAlgorithm::DDSketch),
            Arc::new(sealed(
                &FieldDataType::Sketch(SketchAlgorithm::DDSketch),
                Some(SketchParams::DDSketch { alpha: 0.01 }),
            )),
            &quantiles,
        ),
        (
            FieldDataType::Sketch(SketchAlgorithm::KLL),
            Arc::new(sealed(
                &FieldDataType::Sketch(SketchAlgorithm::KLL),
                Some(SketchParams::Kll { k: 1024 }),
            )),
            &quantiles,
        ),
        (
            FieldDataType::Sketch(SketchAlgorithm::CountMinSketch),
            count_min_column(),
            &total,
        ),
    ];
    for (family, original, queries) in sketches {
        let rebuilt = Arc::new(round_trip(&original, &family));
        let expected = merged_by_series(&family, &series, original);
        let actual = merged_by_series(&family, &series, rebuilt);
        assert_eq!(expected.len(), 2);
        for (expected, actual) in expected.iter().zip(&actual) {
            for query in queries {
                assert_eq!(
                    expected.estimate(query).unwrap(),
                    actual.estimate(query).unwrap(),
                    "{family:?} {query:?}"
                );
            }
        }
    }
    let family = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
    let original = Arc::new(sealed(&family, None));
    let rebuilt = Arc::new(round_trip(&original, &family));
    let expected = merged_by_series(&family, &series, original);
    let actual = merged_by_series(&family, &series, rebuilt);
    for (expected, actual) in expected.iter().zip(&actual) {
        assert_eq!(exact_json(expected.as_ref()), exact_json(actual.as_ref()));
    }
}

#[test]
fn malformed_views_are_refused() {
    let family = FieldDataType::Sketch(SketchAlgorithm::DDSketch);
    let (bytes, offsets) = stored(&sealed(
        &family,
        Some(SketchParams::DDSketch { alpha: 0.01 }),
    ));
    // Not a DDSketch view: a KLL column refuses it.
    assert!(StateColumn::from_views(
        FieldDataType::Sketch(SketchAlgorithm::KLL),
        bytes.clone(),
        offsets.clone(),
    )
    .is_err());
    // An offset past the bytes.
    let mut past = offsets.to_vec();
    *past.last_mut().unwrap() += 8;
    assert!(StateColumn::from_views(family.clone(), bytes.clone(), past.into()).is_err());
    // A family with no view layout.
    let hll = FieldDataType::Sketch(SketchAlgorithm::HLL);
    assert!(!family_has_view_layout(&hll));
    assert!(StateColumn::from_views(hll, bytes, offsets).is_err());
}
