//! Exercise the public library without a backend server, store, or scheduler.
use asap_executor::planner::{
    ir::scalar::ColumnRef,
    ir::schema::{
        FieldDataType, GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams, SummaryUpdate,
    },
};
use asap_executor::{factory::create_planner_accumulator, AggregateCore};

fn family(k: u32) -> FieldDataType {
    FieldDataType::Sketch(
        SketchKind::new(
            SketchAlgorithm::Kll,
            SketchParams::Kll {
                k,
                failure_probability: None,
            },
        ),
        GroupingStrategy::PerSubpopulationInstance,
    )
}
fn build(values: &[f64]) -> Box<dyn AggregateCore> {
    let mut operator = create_planner_accumulator(
        &family(512),
        &SummaryUpdate::column(ColumnRef::SampleValue),
        &Default::default(),
    )
    .unwrap();
    for (at, value) in values.iter().enumerate() {
        operator.validate_single_input(*value).unwrap();
        operator.update_single(*value, at as i64).unwrap();
    }
    operator.into_accumulator()
}
fn read(state: &dyn AggregateCore) -> f64 {
    state
        .estimate(&asap_executor::planner::ir::schema::SketchStatistic::Quantile { q: 0.5 })
        .unwrap()
}

// The same kernels work when every build is query-time, when only a prefix
// was precomputed, and when all state was precomputed before the evaluation.
#[test]
fn raw_partial_and_fully_precomputed_use_the_same_kernels() {
    let raw: Vec<f64> = (0..128).map(f64::from).collect();
    let raw_only = build(&raw);
    let stored_prefix = build(&raw[..64]);
    let query_time_suffix = build(&raw[64..]);
    let partial = stored_prefix.merge_with(&*query_time_suffix).unwrap();
    let stored_complete = build(&raw);
    assert_eq!(read(&*raw_only), read(&*partial));
    assert_eq!(read(&*partial), read(&*stored_complete));
    assert!((read(&*raw_only) - 64.0).abs() <= 1.0);
}

// A compiler must reject invalid physical parameters before starting execution.
#[test]
fn invalid_kll_parameters_are_rejected_at_binding() {
    let result = create_planner_accumulator(
        &family(0),
        &SummaryUpdate::column(ColumnRef::SampleValue),
        &Default::default(),
    );
    assert!(result.is_err());
}

// Native CountSketch supports the confidence-sized depth used by the backend;
// a packed-wire column-bit budget must not be imposed on this constructor.
#[test]
fn native_count_sketch_dimensions_are_not_packed_wire_dimensions() {
    use asap_executor::planner::ir::schema::SummaryInputExpr;
    use asap_executor::KeyByLabelValues;
    let family = FieldDataType::Sketch(
        SketchKind::new(
            SketchAlgorithm::CountSketchWithHeap,
            SketchParams::CountSketchWithHeap {
                width: 1200,
                depth: 55,
                heap_size: 3,
            },
        ),
        Default::default(),
    );
    let mut update = SummaryUpdate::column(ColumnRef::SampleValue);
    update.item = Some(SummaryInputExpr::Column(ColumnRef::Named("host".into())));
    let mut operator = create_planner_accumulator(&family, &update, &Default::default()).unwrap();
    let key = KeyByLabelValues::new_with_labels(vec!["a".into()]);
    operator.update_keyed(&key, 7.0, 1000).unwrap();
    let state = operator.into_accumulator();
    let state = state
        .as_any()
        .downcast_ref::<asap_executor::summary_kernels::weighted_frequency::WeightedFrequency>()
        .unwrap();
    assert_eq!(
        state
            .point_estimate(&[asap_executor::values::Value::Utf8("a".into())])
            .unwrap(),
        7.0
    );
}

fn plain_count_sketch(width: u32, depth: u32) -> FieldDataType {
    FieldDataType::Sketch(
        SketchKind::new(
            SketchAlgorithm::CountSketch,
            SketchParams::CountSketch { width, depth },
        ),
        Default::default(),
    )
}

fn host(i: usize) -> asap_executor::KeyByLabelValues {
    asap_executor::KeyByLabelValues::new_with_labels(vec![format!("host-{i}")])
}

/// Build a plain CountSketch over unit events, one per key index.
fn build_plain_count_sketch(
    family: &FieldDataType,
    events: impl IntoIterator<Item = usize>,
) -> Box<dyn AggregateCore> {
    build_weighted_frequency(family, events.into_iter().map(|i| (i, 1.0)))
}

/// Build a keyed frequency summary over `(key index, weight)` updates.
fn build_weighted_frequency(
    family: &FieldDataType,
    updates: impl IntoIterator<Item = (usize, f64)>,
) -> Box<dyn AggregateCore> {
    use asap_executor::planner::ir::schema::SummaryInputExpr;
    let mut update = SummaryUpdate::column(ColumnRef::SampleValue);
    update.item = Some(SummaryInputExpr::Column(ColumnRef::Named("host".into())));
    let mut operator = create_planner_accumulator(family, &update, &Default::default()).unwrap();
    for (at, (i, weight)) in updates.into_iter().enumerate() {
        operator.validate_single_input(weight).unwrap();
        operator.update_keyed(&host(i), weight, at as i64).unwrap();
    }
    operator.into_accumulator()
}

fn plain_estimate(state: &dyn AggregateCore, i: usize) -> f64 {
    use asap_executor::{summary_kernels::weighted_frequency::WeightedFrequency, values::Value};
    state
        .as_any()
        .downcast_ref::<WeightedFrequency>()
        .expect("plain CountSketch state is the per-row-seeded weighted frequency kernel")
        .point_estimate(&[Value::Utf8(format!("host-{i}").into())])
        .unwrap()
}

/// Zipf-like unit event stream: key `i` occurs `1000 / (i + 1) + 1` times.
fn zipf_events(keys: usize) -> Vec<usize> {
    (0..keys)
        .flat_map(|i| std::iter::repeat_n(i, 1000 / (i + 1) + 1))
        .collect()
}

/// Decode, for `keys` keys, the column each row of `family` (a keyed
/// frequency sketch of `width` x `depth`) puts the key in, and assert every
/// pair of rows shares a column for few keys, as independent rows do.
fn assert_rows_hash_independently(family: &FieldDataType, width: usize, depth: usize, keys: usize) {
    // Key `base + p` of a batch carries weight 3^p, so a cell holds a sum
    // whose balanced-ternary digits name the keys that hit it (CountSketch
    // signs give digits -1 or 1, Count-Min only 1).
    const BATCH: usize = 33;
    let mut columns = vec![vec![None; depth]; keys];
    for base in (0..keys).step_by(BATCH) {
        let batch = base..keys.min(base + BATCH);
        let state = build_weighted_frequency(
            family,
            batch.clone().map(|i| (i, 3f64.powi((i - base) as i32))),
        );
        let state = state
            .as_any()
            .downcast_ref::<asap_executor::summary_kernels::weighted_frequency::WeightedFrequency>()
            .expect("the per-row-seeded weighted frequency kernel");
        for (index, cell) in state.counters().iter().enumerate() {
            let (row, column) = (index / width, index % width);
            let mut rest = *cell as i64;
            let mut position = 0;
            while rest != 0 {
                let digit = rest.rem_euclid(3);
                if digit != 0 {
                    let slot = &mut columns[base + position][row];
                    assert!(slot.is_none(), "one cell per row");
                    *slot = Some(column);
                    rest -= if digit == 1 { 1 } else { -1 };
                }
                rest /= 3;
                position += 1;
            }
        }
    }
    let columns = columns
        .into_iter()
        .map(|c| {
            c.into_iter()
                .collect::<Option<Vec<_>>>()
                .expect("one cell per row")
        })
        .collect::<Vec<_>>();
    for a in 0..depth {
        for b in a + 1..depth {
            let shared = columns.iter().filter(|c| c[a] == c[b]).count();
            // Independent rows share a column for keys / width < 1 key per pair.
            assert!(shared <= 5, "rows {a} and {b} share {shared} columns");
        }
    }
}

// A deep plain CountSketch, sized from a small δ, builds and keeps every row
// independent: each row hashes with its own seed, so no row geometry limit
// applies and two rows rarely put a key in the same column.
#[test]
fn deep_plain_count_sketch_rows_hash_independently() {
    assert_rows_hash_independently(&plain_count_sketch(30_000, 15), 30_000, 15, 2_000);
}

fn heap_sketch(
    algorithm: SketchAlgorithm,
    width: u32,
    depth: u32,
    heap_size: u32,
) -> FieldDataType {
    let params = match algorithm {
        SketchAlgorithm::CmsWithHeap => SketchParams::CmsWithHeap {
            width,
            depth,
            heap_size,
        },
        _ => SketchParams::CountSketchWithHeap {
            width,
            depth,
            heap_size,
        },
    };
    FieldDataType::Sketch(SketchKind::new(algorithm, params), Default::default())
}

// The factory's top-k sketches use the same per-row-seeded kernel: past 20
// rows (where a fixed seed list would repeat) every row stays independent.
#[test]
fn deep_heap_sketch_rows_hash_independently() {
    for algorithm in [
        SketchAlgorithm::CmsWithHeap,
        SketchAlgorithm::CountSketchWithHeap,
    ] {
        assert_rows_hash_independently(&heap_sketch(algorithm, 30_000, 25, 4), 30_000, 25, 1_000);
    }
}

// A factory-built top-k state is the DAG's state type, so it loads as a typed
// summary value, and it keeps fractional weights exactly.
#[test]
fn heap_sketch_states_load_as_dag_summary_values() {
    use asap_executor::{
        planner::ir::schema::{Field, Schema},
        summary_kernels::weighted_frequency::WeightedFrequency,
        values::{Batch, Value},
    };
    for algorithm in [
        SketchAlgorithm::CmsWithHeap,
        SketchAlgorithm::CountSketchWithHeap,
    ] {
        let family = heap_sketch(algorithm, 1_024, 5, 4);
        let state = build_weighted_frequency(&family, [(0, 0.25), (0, 0.5), (1, 2.0)]);
        let estimate = state
            .as_any()
            .downcast_ref::<WeightedFrequency>()
            .expect("the DAG's weighted frequency state")
            .point_estimate(&[Value::Utf8("host-0".into())])
            .unwrap();
        assert_eq!(estimate, 0.75);
        let schema = std::sync::Arc::new(Schema {
            fields: vec![Field {
                name: "state".into(),
                dtype: family.clone(),
                nullable: false,
                table: None,
            }],
            unique_keys: vec![],
            closed: false,
            time_index: None,
        });
        let row = vec![Value::Summary {
            family,
            state: std::sync::Arc::from(state),
        }];
        assert!(Batch::try_new(schema, vec![row]).is_ok());
    }
}

// A factory-built top-k state and the state the DAG's keyed build makes
// (`WeightedFrequency::new`, updated with the key's `Utf8` label values) are
// interchangeable: merged either way, they equal one build over both inputs.
#[test]
fn factory_and_dag_heap_states_are_interchangeable() {
    use asap_executor::summary_kernels::weighted_frequency::FrequencyAlgorithm;
    use asap_executor::{summary_kernels::weighted_frequency::WeightedFrequency, values::Value};
    let estimate = |state: &dyn AggregateCore, i: usize| {
        state
            .as_any()
            .downcast_ref::<WeightedFrequency>()
            .expect("the DAG's weighted frequency state")
            .point_estimate(&[Value::Utf8(format!("host-{i}").into())])
            .unwrap()
    };
    for (algorithm, frequency) in [
        (SketchAlgorithm::CmsWithHeap, FrequencyAlgorithm::Cms),
        (
            SketchAlgorithm::CountSketchWithHeap,
            FrequencyAlgorithm::CountSketch,
        ),
    ] {
        let family = heap_sketch(algorithm.clone(), 1_024, 5, 4);
        let factory = build_weighted_frequency(&family, [(0, 1.0), (1, 2.0)]);
        let mut dag = WeightedFrequency::new(frequency, 1_024, 5, 4).unwrap();
        dag.update(&[Value::Utf8("host-0".into())], 3.0).unwrap();
        let union = build_weighted_frequency(&family, [(0, 1.0), (1, 2.0), (0, 3.0)]);
        for merged in [
            factory.merge_with(&dag).unwrap(),
            dag.merge_with(factory.as_ref()).unwrap(),
        ] {
            for i in 0..2 {
                assert_eq!(
                    estimate(merged.as_ref(), i),
                    estimate(union.as_ref(), i),
                    "{algorithm:?} host-{i}"
                );
            }
        }
    }
}

// Count-Min needs nonnegative weights; a negative input is an error at
// validation, never silently dropped.
#[test]
fn count_min_heap_rejects_negative_inputs() {
    use asap_executor::planner::ir::schema::SummaryInputExpr;
    let mut update = SummaryUpdate::column(ColumnRef::SampleValue);
    update.item = Some(SummaryInputExpr::Column(ColumnRef::Named("host".into())));
    let family = heap_sketch(SketchAlgorithm::CmsWithHeap, 64, 3, 4);
    let operator = create_planner_accumulator(&family, &update, &Default::default()).unwrap();
    assert!(operator.validate_single_input(-1.0).is_err());
    assert!(operator.validate_single_input(0.5).is_ok());
}

// The planner certifies `|estimate − truth| ≤ √(3/w) ‖f‖₂` for every point
// query of a plain CountSketch; a deep shape meets it on a skewed stream.
#[test]
fn deep_plain_count_sketch_estimates_meet_the_certified_bound() {
    let width = 30_000u32;
    let keys = 3_000;
    let events = zipf_events(keys);
    let state = build_plain_count_sketch(&plain_count_sketch(width, 15), events.iter().copied());
    let mut truth = vec![0.0f64; keys];
    for &i in &events {
        truth[i] += 1.0;
    }
    let l2 = truth.iter().map(|f| f * f).sum::<f64>().sqrt();
    let bound = (3.0 / f64::from(width)).sqrt() * l2;
    for (i, truth) in truth.iter().enumerate() {
        let error = (plain_estimate(&*state, i) - truth).abs();
        assert!(error <= bound, "key {i}: error {error} > bound {bound}");
    }
}

// Panes merge cell by cell: every pane seeds row `r` the same way, so merging
// two panes equals building the union in one pane.
#[test]
fn plain_count_sketch_merge_equals_union_build() {
    let family = plain_count_sketch(1_001, 7);
    let events = zipf_events(500);
    let (left, right) = events.split_at(events.len() / 3);
    let merged = build_plain_count_sketch(&family, left.iter().copied())
        .merge_with(&*build_plain_count_sketch(&family, right.iter().copied()))
        .unwrap();
    let union = build_plain_count_sketch(&family, events.iter().copied());
    let cells = |state: &dyn AggregateCore| {
        state
            .as_any()
            .downcast_ref::<asap_executor::summary_kernels::weighted_frequency::WeightedFrequency>()
            .unwrap()
            .counters()
            .to_vec()
    };
    assert_eq!(cells(&*merged), cells(&*union));
    for i in 0..500 {
        assert_eq!(plain_estimate(&*merged, i), plain_estimate(&*union, i));
    }
    // A different shape is a different family and does not merge.
    assert!(merged
        .merge_with(&*build_plain_count_sketch(
            &plain_count_sketch(1_001, 5),
            [0]
        ))
        .is_err());
}

// A keyed weighted-frequency update whose sum would pass `f64::MAX` returns
// an error from `update_keyed` instead of panicking; the state keeps the
// last good sum.
#[test]
fn keyed_update_overflow_is_an_error_not_a_panic() {
    use asap_executor::planner::ir::schema::SummaryInputExpr;
    let mut update = SummaryUpdate::column(ColumnRef::SampleValue);
    update.item = Some(SummaryInputExpr::Column(ColumnRef::Named("host".into())));
    let family = plain_count_sketch(64, 3);
    let mut operator = create_planner_accumulator(&family, &update, &Default::default()).unwrap();
    operator.validate_single_input(f64::MAX).unwrap();
    operator.update_keyed(&host(0), f64::MAX, 0).unwrap();
    let error = operator.update_keyed(&host(0), f64::MAX, 1).unwrap_err();
    assert!(error.contains("overflow"), "{error}");
    let state = operator.into_accumulator();
    assert_eq!(plain_estimate(&*state, 0), f64::MAX);
}
