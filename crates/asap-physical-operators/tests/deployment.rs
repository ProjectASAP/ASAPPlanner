//! Exercise the public library without a backend server, store, or scheduler.
use asap_physical_operators::planner::{
    post_asap::{
        FieldDataType, GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams, SummaryUpdate,
    },
    pre_asap::ColumnRef,
};
use asap_physical_operators::{factory::create_planner_accumulator, AggregateCore};

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
        operator.update_single(*value, at as i64);
    }
    operator.into_accumulator()
}
fn read(state: &dyn AggregateCore) -> f64 {
    state
        .estimate(
            &asap_physical_operators::planner::post_asap::SketchStatistic::Quantile { q: 0.5 },
        )
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
    use asap_physical_operators::planner::post_asap::SummaryInputExpr;
    use asap_physical_operators::KeyByLabelValues;
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
    operator.update_keyed(&key, 7.0, 1000);
    let state = operator.into_accumulator();
    let state = state
        .as_any()
        .downcast_ref::<asap_physical_operators::summary_kernels::CountSketchWithHeapAccumulator>()
        .unwrap();
    assert_eq!(state.query_key(&key), 7.0);
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

fn host(i: usize) -> asap_physical_operators::KeyByLabelValues {
    asap_physical_operators::KeyByLabelValues::new_with_labels(vec![format!("host-{i}")])
}

/// Build a plain CountSketch over unit events, one per key index.
fn build_plain_count_sketch(
    family: &FieldDataType,
    events: impl IntoIterator<Item = usize>,
) -> Box<dyn AggregateCore> {
    build_weighted_plain_count_sketch(family, events.into_iter().map(|i| (i, 1.0)))
}

/// Build a plain CountSketch over `(key index, weight)` updates.
fn build_weighted_plain_count_sketch(
    family: &FieldDataType,
    updates: impl IntoIterator<Item = (usize, f64)>,
) -> Box<dyn AggregateCore> {
    use asap_physical_operators::planner::post_asap::SummaryInputExpr;
    let mut update = SummaryUpdate::column(ColumnRef::SampleValue);
    update.item = Some(SummaryInputExpr::Column(ColumnRef::Named("host".into())));
    let mut operator = create_planner_accumulator(family, &update, &Default::default()).unwrap();
    for (at, (i, weight)) in updates.into_iter().enumerate() {
        operator.validate_single_input(weight).unwrap();
        operator.update_keyed(&host(i), weight, at as i64);
    }
    operator.into_accumulator()
}

fn plain_estimate(state: &dyn AggregateCore, i: usize) -> f64 {
    use asap_physical_operators::{
        summary_kernels::weighted_frequency::WeightedFrequency, values::Value,
    };
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

// A deep plain CountSketch, sized from a small δ, builds and keeps every row
// independent: each row hashes with its own seed, so no row geometry limit
// applies and two rows rarely put a key in the same column.
#[test]
fn deep_plain_count_sketch_rows_hash_independently() {
    let (width, depth) = (30_000usize, 15usize);
    let family = plain_count_sketch(width as u32, depth as u32);
    let keys = 2_000;
    // Key `base + p` of a batch carries weight 3^p, so a cell holds a signed
    // sum whose balanced-ternary digits name the keys that hit it.
    const BATCH: usize = 33;
    let mut columns = vec![vec![None; depth]; keys];
    for base in (0..keys).step_by(BATCH) {
        let batch = base..keys.min(base + BATCH);
        let state = build_weighted_plain_count_sketch(
            &family,
            batch.clone().map(|i| (i, 3f64.powi((i - base) as i32))),
        );
        let state = state
            .as_any()
            .downcast_ref::<asap_physical_operators::summary_kernels::weighted_frequency::WeightedFrequency>()
            .unwrap();
        let cells = serde_json::to_value(state).unwrap()["cells"].clone();
        for (index, cell) in cells.as_array().unwrap().iter().enumerate() {
            let (row, column) = (index / width, index % width);
            let mut rest = cell.as_f64().unwrap() as i64;
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
            // Independent rows share a column for 2000 / 30000 < 1 key per pair.
            assert!(shared <= 5, "rows {a} and {b} share {shared} columns");
        }
    }
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
        serde_json::to_value(
            state
                .as_any()
                .downcast_ref::<asap_physical_operators::summary_kernels::weighted_frequency::WeightedFrequency>()
                .unwrap(),
        )
        .unwrap()["cells"]
            .clone()
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
