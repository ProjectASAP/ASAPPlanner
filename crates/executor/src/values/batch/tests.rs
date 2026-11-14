use super::*;
use crate::summary_kernels::DDSketchAccumulator;
use planner_types::ir::schema::{DataType, Field, Schema};
use planner_types::summary_type::SketchAlgorithm;

fn field(name: &str, dtype: DataType, nullable: bool) -> Field {
    Field {
        name: name.into(),
        dtype: FieldDataType::Plain(dtype),
        nullable,
    }
}

fn schema(fields: Vec<Field>) -> SchemaRef {
    Arc::new(Schema {
        fields,
        time_index: None,
        unique_keys: vec![],
        closed: false,
    })
}

/// Exact equality, including float bits (`Value`'s key merges zero signs).
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float64(a), Value::Float64(b)) => a.to_bits() == b.to_bits(),
        (Value::List(a), Value::List(b)) | (Value::Struct(a), Value::Struct(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(a, b)| same(a, b))
        }
        (Value::Map(a), Value::Map(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b.iter())
                    .all(|((ak, av), (bk, bv))| same(ak, bk) && same(av, bv))
        }
        (Value::Summary { state: a, .. }, Value::Summary { state: b, .. }) => Arc::ptr_eq(a, b),
        _ => format!("{a:?}") == format!("{b:?}"),
    }
}

/// The rows as values.
fn decoded(batch: &Batch) -> Vec<Vec<Value>> {
    batch.to_rows()
}

// Every Planner type survives the trip into Arrow columns and back,
// nulls, NaN and negative zero included.
#[test]
fn rows_round_trip_through_arrow_columns() {
    let element = Field::<DataType>::new("item", DataType::Float64, true);
    let inner = vec![
        Field::<DataType> {
            name: "a".into(),
            dtype: DataType::Int64,
            nullable: false,
        },
        Field::<DataType> {
            name: "b".into(),
            dtype: DataType::Utf8,
            nullable: true,
        },
    ];
    let labels = DataType::Map {
        key: Box::new(DataType::Utf8),
        value: Box::new(DataType::Utf8),
        value_nullable: true,
    };
    let schema = schema(vec![
        field("null", DataType::Null, true),
        field("bool", DataType::Bool, true),
        field("int", DataType::Int64, true),
        field("float", DataType::Float64, false),
        field("text", DataType::Utf8, true),
        field("time", DataType::Timestamp, false),
        field("date", DataType::Date, true),
        field("interval", DataType::Interval, true),
        field(
            "list",
            DataType::List {
                element: Box::new(element),
            },
            true,
        ),
        field("struct", DataType::Struct { fields: inner }, true),
        field("map", labels, true),
    ]);
    let text = |s: &str| Value::Utf8(s.into());
    let rows = vec![
        vec![
            Value::Null,
            Value::Bool(true),
            Value::Int64(-3),
            Value::Float64(-0.0),
            text("api"),
            Value::Timestamp(1_000),
            Value::Date(7),
            Value::Interval {
                months: 1,
                days: 2,
                nanos: 3,
            },
            Value::List(vec![Value::Float64(f64::NAN), Value::Float64(1.5)].into()),
            Value::Struct(vec![Value::Int64(1), Value::Null].into()),
            Value::Map(vec![(text("job"), text("api")), (text("x"), Value::Null)].into()),
        ],
        vec![
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Float64(f64::INFINITY),
            Value::Null,
            Value::Timestamp(-5),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ],
        vec![
            Value::Null,
            Value::Bool(false),
            Value::Int64(i64::MAX),
            Value::Float64(2.0),
            text("api"),
            Value::Timestamp(0),
            Value::Date(-1),
            Value::Interval {
                months: 0,
                days: 0,
                nanos: 0,
            },
            Value::List(Arc::from([])),
            Value::Struct(vec![Value::Int64(2), text("")].into()),
            Value::Map(Arc::from([])),
        ],
    ];
    let batch = Batch::try_new(schema, rows.clone()).unwrap();
    assert_eq!(batch.num_rows(), 3);
    let back = decoded(&batch);
    for (expected, actual) in rows.iter().zip(&back) {
        for (e, a) in expected.iter().zip(actual) {
            assert!(same(e, a), "{e:?} became {a:?}");
        }
    }
}

// A batch text column is a dictionary with one entry per distinct value.
#[test]
fn text_columns_are_dictionary_encoded() {
    use arrow::array::{AsArray, DictionaryArray};
    let schema = schema(vec![field("job", DataType::Utf8, false)]);
    let rows = ["api", "web", "api", "api", "web"]
        .iter()
        .map(|job| vec![Value::Utf8((*job).into())])
        .collect();
    let batch = Batch::try_new(schema, rows).unwrap();
    let column: &DictionaryArray<arrow::datatypes::UInt32Type> = batch.column(0).as_dictionary();
    assert_eq!(column.values().len(), 2);
    assert_eq!(column.keys().values().to_vec(), vec![0, 1, 0, 0, 1]);
}

fn dd_family() -> FieldDataType {
    FieldDataType::Sketch(SketchAlgorithm::DDSketch)
}

// Rows sharing a state keep sharing one state; filtering rows shares the
// state column instead of copying sketches.
#[test]
fn state_columns_hold_each_state_once_and_filter_shares_them() {
    let schema = Arc::new(Schema {
        fields: vec![
            field("job", DataType::Utf8, false),
            Field {
                name: "state".into(),
                dtype: dd_family(),
                nullable: false,
            },
        ],
        time_index: None,
        unique_keys: vec![],
        closed: false,
    });
    let shared: Arc<dyn AggregateCore> = Arc::new(DDSketchAccumulator::new(0.01));
    let other: Arc<dyn AggregateCore> = Arc::new(DDSketchAccumulator::new(0.01));
    let state = |s: &Arc<dyn AggregateCore>| Value::Summary {
        family: dd_family(),
        state: s.clone(),
    };
    let rows = vec![
        vec![Value::Utf8("a".into()), state(&shared)],
        vec![Value::Utf8("b".into()), state(&other)],
        vec![Value::Utf8("c".into()), state(&shared)],
    ];
    let batch = Batch::try_new(schema, rows).unwrap();
    let states = batch.state_column(1).unwrap();
    assert_eq!(states.len(), 2);
    assert!(batch.state_column(0).is_none());
    let kept = batch
        .filter(&BooleanArray::from(vec![true, false, true]))
        .unwrap();
    assert!(Arc::ptr_eq(kept.state_column(1).unwrap(), states));
    let rows = decoded(&kept);
    assert_eq!(rows.len(), 2);
    assert!(same(&rows[0][1], &state(&shared)) && same(&rows[1][1], &state(&shared)));
}

// Memory is counted per batch from its buffers; zero-width rows still
// cost their slot.
#[test]
fn zero_width_rows_consume_memory() {
    let empty = schema(vec![]);
    let none = Batch::try_new(empty.clone(), vec![]).unwrap();
    let many = Batch::try_new(empty, vec![vec![]; 200]).unwrap();
    assert_eq!(many.num_rows(), 200);
    assert_eq!(
        many.memory_bytes() - none.memory_bytes(),
        200 * ROW_SLOT_BYTES
    );
}

// A runtime can share batches, stored tumbling windows included, across threads.
#[test]
fn batches_and_state_columns_are_send_and_sync() {
    fn shareable<T: Send + Sync>() {}
    shareable::<Batch>();
    shareable::<StateColumn>();
}

// A value of the wrong type or width is rejected, as before.
#[test]
fn rows_must_match_the_schema() {
    let schema = schema(vec![field("v", DataType::Int64, false)]);
    assert!(Batch::try_new(schema.clone(), vec![vec![Value::Null]]).is_err());
    assert!(Batch::try_new(schema.clone(), vec![vec![Value::Float64(1.)]]).is_err());
    assert!(Batch::try_new(schema, vec![vec![]]).is_err());
}

// A runtime turns sealed per-sample states and their group columns into
// a batch: the states and labels read back as the rows say.
#[test]
fn a_batch_from_columns_holds_sealed_states_and_labels() {
    use crate::StateColumnUpdater;
    use planner_types::summary_type::{ExactKind, ExactParams};
    let family = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
    let mut updater = StateColumnUpdater::new(&family, None).unwrap();
    updater.ensure_groups(2).unwrap();
    updater.update(0, 1.5, 0).unwrap();
    updater.update(1, 4.0, 0).unwrap();
    updater.update(0, 2.0, 0).unwrap();
    let states = Arc::new(updater.seal());
    let schema = Arc::new(Schema {
        fields: vec![
            field("job", DataType::Utf8, false),
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
    let jobs: ArrayRef = Arc::new(
        ["api", "web"]
            .into_iter()
            .collect::<arrow::array::DictionaryArray<arrow::datatypes::UInt32Type>>(),
    );
    let batch = Batch::try_from_columns(
        schema.clone(),
        vec![
            BatchColumn::Plain(jobs.clone()),
            BatchColumn::States {
                indices: UInt32Array::from(vec![1, 0]),
                states: states.clone(),
            },
        ],
    )
    .unwrap();
    let rows = batch.to_rows();
    let sum = |value: &Value| {
        let Value::Summary { state, .. } = value else {
            panic!()
        };
        state
            .as_any()
            .downcast_ref::<crate::summary_kernels::exact::ExactAccumulator>()
            .unwrap()
            .evaluation(planner_types::summary_type::ExactKind::Sum, None, None)
            .unwrap()
            .unwrap()
    };
    assert_eq!(
        format!("{:?}", rows[0][0]),
        format!("{:?}", Value::Utf8("api".into()))
    );
    assert_eq!((sum(&rows[0][1]), sum(&rows[1][1])), (4.0, 3.5));
    // Bad input: a column of another layout, a state index past the
    // column, another family, or columns of different lengths.
    let plain_text: ArrayRef = Arc::new(arrow::array::StringArray::from(vec!["api", "web"]));
    let indices = |values: Vec<u32>| BatchColumn::States {
        indices: UInt32Array::from(values),
        states: states.clone(),
    };
    let other_family = Arc::new(
        StateColumnUpdater::new(
            &FieldDataType::ExactAggregate(ExactKind::Count, ExactParams::Count),
            None,
        )
        .unwrap()
        .seal(),
    );
    for columns in [
        vec![BatchColumn::Plain(plain_text), indices(vec![0, 1])],
        vec![BatchColumn::Plain(jobs.clone()), indices(vec![0, 2])],
        vec![
            BatchColumn::Plain(jobs.clone()),
            BatchColumn::States {
                indices: UInt32Array::from(vec![0, 0]),
                states: other_family,
            },
        ],
        vec![BatchColumn::Plain(jobs.clone()), indices(vec![0])],
        vec![indices(vec![0, 1]), BatchColumn::Plain(jobs.clone())],
    ] {
        assert!(matches!(
            Batch::try_from_columns(schema.clone(), columns),
            Err(Error::Invalid(_))
        ));
    }
}

// A NULL among a dictionary's values is a NULL in the column: refused
// in a non-nullable field, not read back as "".
#[test]
fn a_null_dictionary_value_is_a_null() {
    use arrow::array::{DictionaryArray, StringArray};
    let schema = Arc::new(Schema {
        fields: vec![field("job", DataType::Utf8, false)],
        time_index: None,
        unique_keys: vec![],
        closed: false,
    });
    let jobs = DictionaryArray::<arrow::datatypes::UInt32Type>::try_new(
        UInt32Array::from(vec![0, 1]),
        Arc::new(StringArray::from(vec![Some("a"), None])),
    )
    .unwrap();
    assert!(matches!(
        Batch::try_from_columns(schema, vec![BatchColumn::Plain(Arc::new(jobs.clone()))]),
        Err(Error::Invalid(_))
    ));
    // A nullable field refuses it too: a NULL is a NULL key, so every
    // dictionary read finds a text. A NULL key there reads NULL, in rows
    // and in the column's logical nulls.
    let nullable = Arc::new(Schema {
        fields: vec![field("job", DataType::Utf8, true)],
        time_index: None,
        unique_keys: vec![],
        closed: false,
    });
    assert!(matches!(
        Batch::try_from_columns(nullable.clone(), vec![BatchColumn::Plain(Arc::new(jobs))]),
        Err(Error::Invalid(_))
    ));
    let null_key = DictionaryArray::<arrow::datatypes::UInt32Type>::try_new(
        UInt32Array::from(vec![Some(0), None]),
        Arc::new(StringArray::from(vec!["a"])),
    )
    .unwrap();
    let batch =
        Batch::try_from_columns(nullable, vec![BatchColumn::Plain(Arc::new(null_key))]).unwrap();
    let rows = batch.to_rows();
    assert!(matches!(&rows[0][0], Value::Utf8(job) if job.as_ref() == "a"));
    assert!(matches!(rows[1][0], Value::Null));
    let nulls = batch.column(0).logical_nulls().unwrap();
    assert!(nulls.is_valid(0) && nulls.is_null(1));
}

// A filter that keeps few rows keeps only their states: a tumbling window filtered
// to two of ten series holds two states, not ten.
#[test]
fn a_selective_filter_compacts_its_state_column() {
    use crate::StateColumnUpdater;
    use planner_types::summary_type::{ExactKind, ExactParams};
    let family = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
    let mut updater = StateColumnUpdater::new(&family, None).unwrap();
    updater.ensure_groups(10).unwrap();
    for group in 0..10 {
        updater.update(group, f64::from(group), 0).unwrap();
    }
    let schema = Arc::new(Schema {
        fields: vec![
            field("k", DataType::Int64, false),
            Field {
                name: "state".into(),
                dtype: family,
                nullable: false,
            },
        ],
        time_index: None,
        unique_keys: vec![],
        closed: false,
    });
    let batch = Batch::try_from_columns(
        schema,
        vec![
            BatchColumn::Plain(Arc::new(arrow::array::Int64Array::from_iter_values(0..10))),
            BatchColumn::States {
                indices: UInt32Array::from_iter_values(0..10),
                states: Arc::new(updater.seal()),
            },
        ],
    )
    .unwrap();
    let keep = BooleanArray::from((0..10).map(|i| i == 3 || i == 7).collect::<Vec<_>>());
    let kept = batch.filter(&keep).unwrap();
    assert_eq!(kept.state_column(1).unwrap().len(), 2);
    assert!(kept.memory_bytes() < batch.memory_bytes());
    // The rows still read their own states.
    let sums = decoded(&kept)
        .iter()
        .map(|row| {
            let Value::Summary { state, .. } = &row[1] else {
                panic!()
            };
            state
                .as_any()
                .downcast_ref::<crate::summary_kernels::exact::ExactAccumulator>()
                .unwrap()
                .evaluation(planner_types::summary_type::ExactKind::Sum, None, None)
                .unwrap()
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(sums, vec![3.0, 7.0]);
}

// Two batches over different exact state columns join into one exact
// column of the states their rows reference, each once, still records.
#[test]
fn concat_of_different_exact_columns_keeps_records() {
    use crate::StateColumnUpdater;
    use planner_types::summary_type::{ExactKind, ExactParams};
    let family = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
    let schema = Arc::new(Schema {
        fields: vec![Field {
            name: "state".into(),
            dtype: family.clone(),
            nullable: false,
        }],
        time_index: None,
        unique_keys: vec![],
        closed: false,
    });
    // States [10, 20, 30] referenced as rows [2, 0, 2]; states [1, 2]
    // referenced as rows [1].
    let batch = |sums: &[f64], rows: Vec<u32>| {
        let mut updater = StateColumnUpdater::new(&family, None).unwrap();
        updater.ensure_groups(sums.len()).unwrap();
        for (group, &sum) in sums.iter().enumerate() {
            updater.update(group as u32, sum, 0).unwrap();
        }
        Batch::try_from_columns(
            schema.clone(),
            vec![BatchColumn::States {
                indices: UInt32Array::from(rows),
                states: Arc::new(updater.seal()),
            }],
        )
        .unwrap()
    };
    let first = batch(&[10., 20., 30.], vec![2, 0, 2]);
    let second = batch(&[1., 2.], vec![1]);
    assert!(matches!(
        first.state_column(0).unwrap().storage(),
        super::super::StateStorage::Exact(_)
    ));
    let joined = Batch::concat(&schema, &[first, second]).unwrap();
    let states = joined.state_column(0).unwrap();
    let super::super::StateStorage::Exact(records) = states.storage() else {
        panic!("exact states stay records");
    };
    assert_eq!(records.len(), 3);
    let indices = joined
        .column(0)
        .as_any()
        .downcast_ref::<UInt32Array>()
        .unwrap();
    assert_eq!(indices.values().to_vec(), vec![0, 1, 0, 2]);
    let sums = decoded(&joined)
        .iter()
        .map(|row| {
            let Value::Summary { state, .. } = &row[0] else {
                panic!()
            };
            state
                .as_any()
                .downcast_ref::<crate::summary_kernels::exact::ExactAccumulator>()
                .unwrap()
                .evaluation(planner_types::summary_type::ExactKind::Sum, None, None)
                .unwrap()
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(sums, vec![30.0, 10.0, 30.0, 2.0]);
}

/// A batch of `count` rows, one state each: KLL (k = 200) objects, or
/// exact sums.
fn state_batch(count: u32, exact: bool) -> Batch {
    use crate::StateColumnUpdater;
    use planner_types::summary_type::{ExactKind, ExactParams, SketchAlgorithm};
    let (family, parameters) = if exact {
        (
            FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
            None,
        )
    } else {
        (
            FieldDataType::Sketch(SketchAlgorithm::Kll),
            Some(planner_types::summary_type::SketchParams::Kll { k: 200 }),
        )
    };
    let mut updater = StateColumnUpdater::new(&family, parameters.as_ref()).unwrap();
    updater.ensure_groups(count as usize).unwrap();
    for group in 0..count {
        updater.update(group, f64::from(group), 0).unwrap();
    }
    let schema = Arc::new(Schema {
        fields: vec![Field {
            name: "state".into(),
            dtype: family,
            nullable: false,
        }],
        time_index: None,
        unique_keys: vec![],
        closed: false,
    });
    Batch::try_from_columns(
        schema,
        vec![BatchColumn::States {
            indices: UInt32Array::from_iter_values(0..count),
            states: Arc::new(updater.seal()),
        }],
    )
    .unwrap()
}

/// The bytes `batch` is charged for its state columns.
fn charged_states(batch: &Batch) -> usize {
    batch
        .state_columns
        .iter()
        .flatten()
        .map(|column| column.charged_state_bytes())
        .sum()
}

// Compacting or concatenating stored (held outside the run) object
// columns shares their sketches by pointer: the new column is charged
// its pointer slots, not the sketches.
#[test]
fn derived_columns_of_stored_sketches_charge_only_pointers() {
    let stored = state_batch(100, false);
    stored.mark_states_held_outside_run();
    // Each derived state: its pointer slot and its "held outside" bit.
    let pointers =
        |states: usize| states * (std::mem::size_of::<Arc<dyn crate::AggregateCore>>() + 1);
    // Compaction: 10 of 100 rows kept.
    let keep = BooleanArray::from((0..100).map(|i| i % 10 == 0).collect::<Vec<_>>());
    let kept = stored.filter(&keep).unwrap();
    assert_eq!(kept.state_column(0).unwrap().len(), 10);
    assert_eq!(charged_states(&kept), pointers(10));
    // Concat of two different stored columns.
    let other = state_batch(50, false);
    other.mark_states_held_outside_run();
    let joined = Batch::concat(stored.schema(), &[stored.clone(), other]).unwrap();
    assert_eq!(joined.state_column(0).unwrap().len(), 150);
    assert_eq!(charged_states(&joined), pointers(150));
    // A run-built column concatenated with a stored one: only the
    // run-built sketches are charged in full.
    let built = state_batch(50, false);
    let built_bytes = charged_states(&built);
    let mixed = Batch::concat(&stored.schema().clone(), &[stored, built]).unwrap();
    assert_eq!(charged_states(&mixed), pointers(150) + built_bytes);
}

// Exact records are copied, so compacting or concatenating stored exact
// columns is charged for the copied records.
#[test]
fn derived_columns_of_stored_exact_records_charge_the_copies() {
    let stored = state_batch(100, true);
    stored.mark_states_held_outside_run();
    assert_eq!(charged_states(&stored), 0);
    let record = std::mem::size_of::<crate::summary_kernels::exact::ScalarState>();
    let keep = BooleanArray::from((0..100).map(|i| i % 10 == 0).collect::<Vec<_>>());
    let kept = stored.filter(&keep).unwrap();
    assert_eq!(charged_states(&kept), 10 * record);
    let other = state_batch(50, true);
    other.mark_states_held_outside_run();
    let joined = Batch::concat(&stored.schema().clone(), &[stored, other]).unwrap();
    assert!(charged_states(&joined) >= 150 * record);
}

// Deriving twice keeps each sketch's "held outside the run" bit: concat
// then compaction, or compaction then concat, of stored sketches is
// charged only pointers; run-built sketches stay charged in full.
#[test]
fn derived_twice_stored_sketches_still_charge_only_pointers() {
    let slot = std::mem::size_of::<Arc<dyn crate::AggregateCore>>();
    let stored = |count| {
        let batch = state_batch(count, false);
        batch.mark_states_held_outside_run();
        batch
    };
    let every_other =
        |rows: usize| BooleanArray::from((0..rows).map(|i| i % 10 == 0).collect::<Vec<_>>());
    // Concat, then compaction.
    let (a, b) = (stored(100), stored(100));
    let schema = a.schema().clone();
    let joined = Batch::concat(&schema, &[a, b]).unwrap();
    let kept = joined.filter(&every_other(200)).unwrap();
    assert_eq!(kept.state_column(0).unwrap().len(), 20);
    assert!(charged_states(&kept) <= 20 * (slot + 1));
    // Compaction, then concat.
    let (a, b) = (stored(100), stored(100));
    let (a, b) = (
        a.filter(&every_other(100)).unwrap(),
        b.filter(&every_other(100)).unwrap(),
    );
    let joined = Batch::concat(&schema, &[a, b]).unwrap();
    assert_eq!(joined.state_column(0).unwrap().len(), 20);
    assert!(charged_states(&joined) <= 20 * (slot + 1));
    // Mixed: 50 run-built and 100 stored sketches, concatenated, then
    // compacted to every tenth row (5 run-built, 10 stored), then
    // concatenated with another compacted stored batch (10 stored).
    let built = state_batch(50, false);
    let one_built = charged_states(&built) / 50;
    let mixed = Batch::concat(&schema, &[built, stored(100)]).unwrap();
    let kept = mixed.filter(&every_other(150)).unwrap();
    assert_eq!(kept.state_column(0).unwrap().len(), 15);
    let charged = charged_states(&kept);
    assert!(charged >= 5 * one_built && charged <= 5 * one_built + 15 * (slot + 1));
    let other = stored(100).filter(&every_other(100)).unwrap();
    let joined = Batch::concat(&schema, &[kept, other]).unwrap();
    assert_eq!(joined.state_column(0).unwrap().len(), 25);
    let charged = charged_states(&joined);
    assert!(charged >= 5 * one_built && charged <= 5 * one_built + 25 * (slot + 1));
}

// An index past a derived column reads "not held outside the run", so a
// wrong index charges the state in full rather than undercharging.
#[test]
fn an_index_past_a_derived_column_is_not_held_outside_the_run() {
    let stored = state_batch(4, false);
    stored.mark_states_held_outside_run();
    let kept = stored
        .filter(&BooleanArray::from(vec![true, false, false, false]))
        .unwrap();
    let column = kept.state_column(0).unwrap();
    assert_eq!(column.len(), 1);
    assert!(column.state_held_outside_run(0));
    assert!(!column.state_held_outside_run(1));
}
