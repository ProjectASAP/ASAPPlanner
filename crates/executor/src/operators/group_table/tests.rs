//! Group table tests. Expected values come from `ReferenceGroupTable` (the
//! table before group ids stopped hashing every row), from a `BTreeMap`
//! grouping by `Value::key`, or from key bytes worked out by hand; never
//! from `GroupTable` itself.
use super::reference::ReferenceGroupTable;
use super::*;
use crate::operators::common::{row_chunks, ROWS_PER_CHUNK};
use crate::values::{BatchColumn, SchemaRef};
use arrow::array::UInt32Array;
use planner_types::ir::schema::{Field, Schema};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

/// A nullable plain field.
fn field(name: &str, dtype: DataType) -> Field {
    Field {
        name: name.into(),
        dtype: FieldDataType::Plain(dtype),
        nullable: true,
    }
}

fn schema_of(fields: Vec<Field>) -> SchemaRef {
    Arc::new(Schema {
        fields,
        time_index: None,
        unique_keys: vec![],
        closed: false,
    })
}

/// A dictionary text column: `entries` are the dictionary's texts and
/// `keys` each row's entry (`None` is NULL). Example: entries ["b", "a"]
/// and keys [1, 0, 1] hold "a", "b", "a".
fn texts(entries: &[&str], keys: &[Option<u32>]) -> ArrayRef {
    texts_over(Arc::new(StringArray::from(entries.to_vec())), keys)
}

/// A dictionary text column over an existing dictionary `entries`.
fn texts_over(entries: ArrayRef, keys: &[Option<u32>]) -> ArrayRef {
    Arc::new(
        DictionaryArray::<UInt32Type>::try_new(UInt32Array::from(keys.to_vec()), entries).unwrap(),
    )
}

/// A column of `dtype` holding `values`.
fn plain(dtype: &DataType, values: &[Value]) -> ArrayRef {
    arrow_columns::encode(
        dtype,
        ColumnDepth::BatchColumn,
        &values.iter().collect::<Vec<_>>(),
    )
    .unwrap()
}

fn batch_of(schema: &SchemaRef, columns: Vec<ArrayRef>) -> Batch {
    Batch::try_from_columns(
        schema.clone(),
        columns.into_iter().map(BatchColumn::Plain).collect(),
    )
    .unwrap()
}

/// `batches` cut into the slices grouped operators feed the table.
fn chunked(batches: &[Batch]) -> Vec<Batch> {
    batches.iter().flat_map(row_chunks).collect()
}

/// `texts` as `&str`s, for [`texts`].
fn refs(texts: &[String]) -> Vec<&str> {
    texts.iter().map(String::as_str).collect()
}

fn float_bits(bits: u64) -> f64 {
    f64::from_bits(bits)
}

/// Values equal bit for bit: a Float64 by its bits (so -0.0 differs from
/// 0.0 and NaN payloads differ), every other value by its key bytes, which
/// carry its type tag.
fn same_bits(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float64(a), Value::Float64(b)) => a.to_bits() == b.to_bits(),
        (Value::Float64(_), _) | (_, Value::Float64(_)) => false,
        _ => a.key().unwrap() == b.key().unwrap(),
    }
}

/// Label arrays equal bit for bit, null masks included.
fn assert_same_arrays(new: &[ArrayRef], old: &[ArrayRef], context: &str) {
    assert_eq!(new.len(), old.len(), "{context}");
    for (index, (new, old)) in new.iter().zip(old).enumerate() {
        assert_eq!(new.to_data(), old.to_data(), "{context}: column {index}");
        if let (Some(new), Some(old)) = (
            new.as_any().downcast_ref::<Float64Array>(),
            old.as_any().downcast_ref::<Float64Array>(),
        ) {
            let bits = |array: &Float64Array| {
                array
                    .iter()
                    .map(|value| value.map(f64::to_bits))
                    .collect::<Vec<_>>()
            };
            assert_eq!(bits(new), bits(old), "{context}: column {index}");
        }
    }
}

/// Feeds `batches` to a `GroupTable` and a `ReferenceGroupTable` over
/// `columns` and checks that they agree. After each batch: the same ids
/// (so the same first-appearance numbering) or the same error, the same
/// `len`, and a `memory_bytes` no lower than after the previous batch. At
/// the end: the same key order, the same label of every group in every
/// column (bitwise) and the same label arrays. Returns the new table and
/// the ids of all rows, for checks of concrete values.
fn assert_same_as_reference(
    columns: &[usize],
    batches: &[Batch],
    context: &str,
) -> (GroupTable, Vec<u32>) {
    let mut table = GroupTable::new(columns.to_vec());
    let mut reference = ReferenceGroupTable::new(columns.to_vec());
    let mut ids = Vec::new();
    let mut memory_bytes = table.memory_bytes();
    let mut reference_memory_bytes = reference.memory_bytes();
    for (index, batch) in batches.iter().enumerate() {
        let context = format!("{context}, columns {columns:?}, batch {index}");
        match (table.group_ids(batch), reference.group_ids(batch)) {
            (Ok(new), Ok(old)) => {
                assert_eq!(new, old, "{context}: ids");
                ids.extend(new);
            }
            (Err(new), Err(old)) => {
                assert_eq!(format!("{new:?}"), format!("{old:?}"), "{context}: error");
                return (table, ids);
            }
            (new, old) => panic!("{context}: {new:?} against the reference's {old:?}"),
        }
        assert_eq!(table.len(), reference.len(), "{context}: len");
        assert!(
            table.memory_bytes() >= memory_bytes,
            "{context}: memory_bytes fell from {memory_bytes} to {}",
            table.memory_bytes()
        );
        memory_bytes = table.memory_bytes();
        assert!(
            reference.memory_bytes() >= reference_memory_bytes,
            "{context}: the reference's memory_bytes fell"
        );
        reference_memory_bytes = reference.memory_bytes();
    }
    let context = format!("{context}, columns {columns:?}");
    let order = table.groups_in_key_order();
    assert_eq!(
        order,
        reference.groups_in_key_order(),
        "{context}: key order"
    );
    for group in 0..table.len() as u32 {
        for index in 0..columns.len() {
            let (new, old) = (table.label(group, index), reference.label(group, index));
            assert!(
                same_bits(new, old),
                "{context}: label of group {group} in column {index}: {new:?} against {old:?}"
            );
        }
    }
    if let Some(batch) = batches.first() {
        assert_same_arrays(
            &table.label_arrays(batch.schema(), &order).unwrap(),
            &reference.label_arrays(batch.schema(), &order).unwrap(),
            &context,
        );
    }
    (table, ids)
}

/// A seeded linear congruential generator, as in the oldest test here: no
/// dev-dependency.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as usize
    }

    fn below(&mut self, bound: usize) -> usize {
        self.next() % bound
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            items.swap(i, self.below(i + 1));
        }
    }
}

/// Float64 values equal by key but not by bits, and the edges of the type.
const FLOAT_POOL: [u64; 8] = [
    0x0000_0000_0000_0000, // 0.0
    0x8000_0000_0000_0000, // -0.0
    0x7ff8_0000_0000_0000, // NaN
    0x7ff8_0000_0000_0001, // NaN with a payload
    0xfff8_0000_0000_0000, // -NaN
    0x3ff8_0000_0000_0000, // 1.5
    0x7ff0_0000_0000_0000, // infinity
    0x0000_0000_0000_0001, // the smallest subnormal
];

const TEXT_POOL: [&str; 5] = ["a", "b", "c", "", "dd"];

/// The `draw`th value of a small pool of `dtype` values, so groups repeat.
fn pool_value(dtype: &DataType, draw: usize) -> Value {
    match dtype {
        DataType::Int64 => Value::Int64([0, 1, -1, 256, i64::MIN][draw % 5]),
        DataType::Timestamp => Value::Timestamp([0, 1_000, -1, 256][draw % 4]),
        DataType::Float64 => Value::Float64(float_bits(FLOAT_POOL[draw % FLOAT_POOL.len()])),
        DataType::Bool => Value::Bool(draw.is_multiple_of(2)),
        DataType::Date => Value::Date((draw % 4) as i32 - 1),
        other => unreachable!("no pool of {other:?}"),
    }
}

/// A column of `rows` random rows of `dtype`, NULL in about one row in six
/// when `nullable`. A text column gets a fresh dictionary: the text pool
/// shuffled, some texts entered twice, and some entries no row uses.
fn random_column(rng: &mut Lcg, dtype: &DataType, nullable: bool, rows: usize) -> ArrayRef {
    let null = |rng: &mut Lcg| nullable && rng.below(6) == 0;
    if *dtype == DataType::Utf8 {
        let mut entries = TEXT_POOL.to_vec();
        rng.shuffle(&mut entries);
        for _ in 0..rng.below(3) {
            entries.push(TEXT_POOL[rng.below(TEXT_POOL.len())]);
        }
        let used = entries.len() as u32;
        entries.extend(std::iter::repeat_n("unused", rng.below(3)));
        let keys = (0..rows)
            .map(|_| (!null(rng)).then(|| rng.below(used as usize) as u32))
            .collect::<Vec<_>>();
        return texts(&entries, &keys);
    }
    let values = (0..rows)
        .map(|_| {
            if null(rng) {
                Value::Null
            } else {
                pool_value(dtype, rng.next())
            }
        })
        .collect::<Vec<_>>();
    plain(dtype, &values)
}

// Groups and their order equal the row-at-a-time grouping: a `BTreeMap`
// keyed by each row's `Value::key`s, which merges zero signs and NaN
// payloads and keeps NULL as a group; across batches with different
// dictionaries too.
#[test]
fn groups_equal_value_key_grouping_across_batches() {
    let field = |name: &str, dtype| Field {
        name: name.into(),
        dtype: FieldDataType::Plain(dtype),
        nullable: true,
    };
    let schema = Arc::new(Schema {
        fields: vec![
            field("text", DataType::Utf8),
            field("float", DataType::Float64),
            field("int", DataType::Int64),
            field("flag", DataType::Bool),
            field("time", DataType::Timestamp),
            field("date", DataType::Date),
        ],
        time_index: None,
        unique_keys: vec![],
        closed: false,
    });
    let texts = ["b", "a", "", "a"];
    let floats = [0.0, -0.0, f64::NAN, -f64::NAN, 1.5];
    let mut seed = 3u64;
    let mut next = move || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        (seed >> 33) as usize
    };
    let batches = (0..3)
        .map(|_| {
            let rows = (0..200)
                .map(|_| {
                    let maybe =
                        |n: usize, v: Value| if n.is_multiple_of(7) { Value::Null } else { v };
                    vec![
                        maybe(next(), Value::Utf8(texts[next() % 4].into())),
                        maybe(next(), Value::Float64(floats[next() % 5])),
                        maybe(next(), Value::Int64((next() % 3) as i64 - 1)),
                        maybe(next(), Value::Bool(next().is_multiple_of(2))),
                        Value::Timestamp((next() % 2) as i64),
                        maybe(next(), Value::Date((next() % 2) as i32)),
                    ]
                })
                .collect();
            Batch::try_new(schema.clone(), rows).unwrap()
        })
        .collect::<Vec<_>>();
    for columns in [vec![], vec![0], vec![1], vec![0, 1, 2, 3, 4, 5], vec![5, 3]] {
        let mut table = GroupTable::new(columns.clone());
        let mut reference: BTreeMap<Vec<Vec<u8>>, Vec<usize>> = BTreeMap::new();
        let mut ids = Vec::new();
        let mut row_number = 0;
        for batch in &batches {
            ids.extend(table.group_ids(batch).unwrap());
            for row in &batch.to_rows() {
                let key = columns.iter().map(|&c| row[c].key().unwrap()).collect();
                reference.entry(key).or_default().push(row_number);
                row_number += 1;
            }
        }
        assert_eq!(table.len(), reference.len(), "{columns:?}");
        let order = table.groups_in_key_order();
        for (group, rows) in order.iter().zip(reference.values()) {
            assert!(rows.iter().all(|&row| ids[row] == *group), "{columns:?}");
        }
    }
}

// A group's labels are its own first row's values, as the row operators
// kept them: with two group columns, the group (0.0, "a") seen after
// (-0.0, "b") is labelled 0.0, not the -0.0 its float part was first
// seen with.
#[test]
fn each_group_keeps_its_own_first_values() {
    let field = |name: &str, dtype| Field {
        name: name.into(),
        dtype: FieldDataType::Plain(dtype),
        nullable: false,
    };
    let schema = Arc::new(Schema {
        fields: vec![
            field("float", DataType::Float64),
            field("text", DataType::Utf8),
        ],
        time_index: None,
        unique_keys: vec![],
        closed: false,
    });
    let row = |f: f64, t: &str| vec![Value::Float64(f), Value::Utf8(t.into())];
    let batch = Batch::try_new(
        schema.clone(),
        vec![
            row(-0.0, "b"),
            row(0.0, "a"),
            row(0.0, "b"),
            row(-f64::NAN, "a"),
        ],
    )
    .unwrap();
    for columns in [vec![0, 1], vec![0]] {
        let mut table = GroupTable::new(columns.clone());
        table.group_ids(&batch).unwrap();
        let groups = (0..table.len() as u32).collect::<Vec<_>>();
        let labels = table.label_arrays(&schema, &groups).unwrap();
        let floats = labels[0]
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap()
            .values()
            .iter()
            .map(|v| v.to_bits())
            .collect::<Vec<_>>();
        let expected: &[f64] = if columns.len() == 2 {
            &[-0.0, 0.0, -f64::NAN]
        } else {
            &[-0.0, -f64::NAN]
        };
        assert_eq!(
            floats,
            expected.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            "{columns:?}"
        );
    }
}

// Zero signs and NaN payloads: ±0 is one group and every NaN is one group,
// each labelled with its first row's bits, alone and beside a text column
// in either order.
#[test]
fn float_zero_signs_and_nan_payloads_match_reference() {
    let schema = schema_of(vec![
        field("f", DataType::Float64),
        field("t", DataType::Utf8),
    ]);
    let floats = [
        Some(-0.0),
        Some(0.0),
        Some(float_bits(0x7ff8_0000_0000_0001)),
        Some(float_bits(0xfff8_0000_0000_0000)),
        Some(float_bits(0x7ff0_0000_0000_0001)), // signalling NaN
        Some(f64::INFINITY),
        Some(f64::NEG_INFINITY),
        Some(5e-324),
        None,
        Some(-0.0),
    ];
    let values = floats
        .iter()
        .map(|f| f.map_or(Value::Null, Value::Float64))
        .collect::<Vec<_>>();
    let keys = (0..floats.len() as u32)
        .map(|row| Some(row % 2))
        .collect::<Vec<_>>();
    let batch = batch_of(
        &schema,
        vec![
            plain(&DataType::Float64, &values),
            texts(&["a", "b"], &keys),
        ],
    );
    for columns in [vec![0, 1], vec![1, 0]] {
        assert_same_as_reference(&columns, std::slice::from_ref(&batch), "floats");
    }
    let (table, ids) = assert_same_as_reference(&[0], std::slice::from_ref(&batch), "floats");
    assert_eq!(table.len(), 6);
    assert_eq!(ids, [0, 0, 1, 1, 1, 2, 3, 4, 5, 0]);
    let label_bits = |group| match table.label(group, 0) {
        Value::Float64(v) => v.to_bits(),
        other => panic!("{other:?}"),
    };
    assert_eq!(label_bits(0), 0x8000_0000_0000_0000, "the ±0 group is -0.0");
    assert_eq!(label_bits(1), 0x7ff8_0000_0000_0001, "the first NaN seen");
}

// Groups come out in the byte order of `Value::key` (a type tag, then the
// value's 8 little-endian bytes; NULL is the tag 0 alone), the order of the
// row operators' `BTreeMap`. That order is not numeric: Int64 256 sorts
// before 1. The expected orders are worked out by hand from the key bytes.
#[test]
fn primitive_key_order_is_value_key_byte_order() {
    let ints = |values: &[i64], wrap: fn(i64) -> Value| {
        let mut values = values.iter().map(|&v| wrap(v)).collect::<Vec<_>>();
        values.push(Value::Null);
        values
    };
    let input_ints = [1, 256, -1, 0, i64::MIN, i64::MAX];
    let ordered_ints = [0, i64::MIN, 256, 1, i64::MAX, -1];
    let floats = |values: &[f64]| {
        let mut values = values
            .iter()
            .map(|&v| Value::Float64(v))
            .collect::<Vec<_>>();
        values.push(Value::Null);
        values
    };
    let with_null_first = |mut values: Vec<Value>| {
        values.rotate_right(1);
        values
    };
    let cases = [
        (
            DataType::Int64,
            ints(&input_ints, Value::Int64),
            with_null_first(ints(&ordered_ints, Value::Int64)),
        ),
        (
            DataType::Timestamp,
            ints(&input_ints, Value::Timestamp),
            with_null_first(ints(&ordered_ints, Value::Timestamp)),
        ),
        (
            DataType::Float64,
            floats(&[1.0, 2.0, -1.0, 0.0, f64::NAN]),
            with_null_first(floats(&[0.0, 2.0, 1.0, -1.0, f64::NAN])),
        ),
        (
            DataType::Bool,
            vec![Value::Bool(true), Value::Bool(false), Value::Null],
            vec![Value::Null, Value::Bool(false), Value::Bool(true)],
        ),
    ];
    for (dtype, input, expected) in cases {
        let schema = schema_of(vec![field("v", dtype.clone())]);
        let batch = batch_of(&schema, vec![plain(&dtype, &input)]);
        let context = format!("{dtype:?}");
        let (table, _) = assert_same_as_reference(&[0], std::slice::from_ref(&batch), &context);
        let labels = table
            .groups_in_key_order()
            .into_iter()
            .map(|group| table.label(group, 0).clone())
            .collect::<Vec<_>>();
        assert!(
            labels.len() == expected.len()
                && labels.iter().zip(&expected).all(|(a, b)| same_bits(a, b)),
            "{context}: {labels:?}, expected {expected:?}"
        );
    }
}

// NULL is a group value in every column type, alone or with other columns,
// never skipped: rows with a NULL in each column in turn, all NULL, and a
// column that is NULL in every row.
#[test]
fn nulls_in_every_position_match_reference() {
    let schema = schema_of(vec![
        field("t", DataType::Utf8),
        field("n", DataType::Int64),
        field("f", DataType::Float64),
        field("all_null", DataType::Utf8),
    ]);
    let rows: [(Option<u32>, Option<i64>, Option<f64>); 8] = [
        (Some(0), Some(1), Some(1.5)),
        (None, Some(1), Some(1.5)),
        (Some(0), None, Some(1.5)),
        (Some(0), Some(1), None),
        (None, None, None),
        (Some(1), Some(2), Some(-0.0)),
        (None, Some(2), None),
        (Some(0), Some(1), Some(1.5)),
    ];
    let batch = |rows: &[(Option<u32>, Option<i64>, Option<f64>)]| {
        let keys = rows.iter().map(|row| row.0).collect::<Vec<_>>();
        let ints = rows
            .iter()
            .map(|row| row.1.map_or(Value::Null, Value::Int64))
            .collect::<Vec<_>>();
        let floats = rows
            .iter()
            .map(|row| row.2.map_or(Value::Null, Value::Float64))
            .collect::<Vec<_>>();
        batch_of(
            &schema,
            vec![
                texts(&["a", "b"], &keys),
                plain(&DataType::Int64, &ints),
                plain(&DataType::Float64, &floats),
                texts(&["x"], &vec![None; rows.len()]),
            ],
        )
    };
    let mut reversed = rows;
    reversed.reverse();
    let batches = [batch(&rows), batch(&reversed)];
    for columns in [vec![1], vec![2], vec![2, 1, 0], vec![0, 3], vec![3, 1]] {
        assert_same_as_reference(&columns, &batches, "nulls");
    }
    let (table, _) = assert_same_as_reference(&[0], &batches, "nulls");
    assert_eq!(table.len(), 3, "a, NULL and b");
    let (table, _) = assert_same_as_reference(&[3], &batches, "nulls");
    assert_eq!(table.len(), 1);
    assert!(matches!(table.label(0, 0), Value::Null));
    let (table, _) = assert_same_as_reference(&[0, 1, 2], &batches, "nulls");
    assert_eq!(table.len(), 7, "the 8 rows repeat one tuple");
}

// A batch of no rows adds no group: not with group columns, and not the one
// global group of a table without group columns, even when its dictionary
// has entries. The global group comes with the first row, or from
// `add_empty_group` when no row ever arrives.
#[test]
fn empty_batches_add_no_group() {
    let schema = schema_of(vec![
        field("t", DataType::Utf8),
        field("n", DataType::Int64),
    ]);
    let empty = batch_of(&schema, vec![texts(&[], &[]), plain(&DataType::Int64, &[])]);
    let empty_with_dictionary = batch_of(
        &schema,
        vec![texts(&["a", "b"], &[]), plain(&DataType::Int64, &[])],
    );
    let two_rows = batch_of(
        &schema,
        vec![
            texts(&["a", "b"], &[Some(1), Some(0)]),
            plain(&DataType::Int64, &[Value::Int64(1), Value::Int64(1)]),
        ],
    );
    let sequence = [
        empty.clone(),
        empty_with_dictionary.clone(),
        two_rows,
        empty.clone(),
        empty_with_dictionary.clone(),
    ];
    for (columns, lens) in [(vec![], [0, 0, 1, 1, 1]), (vec![0, 1], [0, 0, 2, 2, 2])] {
        assert_same_as_reference(&columns, &sequence, "empty batches");
        let mut table = GroupTable::new(columns.clone());
        for (batch, len) in sequence.iter().zip(lens) {
            let ids = table.group_ids(batch).unwrap();
            assert_eq!(ids.len(), batch.num_rows(), "{columns:?}");
            assert_eq!(table.len(), len, "{columns:?}");
        }
    }
    let mut table = GroupTable::new(vec![]);
    let mut reference = ReferenceGroupTable::new(vec![]);
    for batch in [&empty, &empty_with_dictionary] {
        assert!(table.group_ids(batch).unwrap().is_empty());
        reference.group_ids(batch).unwrap();
    }
    assert_eq!(table.len(), 0);
    assert_eq!(table.add_empty_group().unwrap(), 0);
    assert_eq!(reference.add_empty_group().unwrap(), 0);
    assert_eq!(table.len(), 1);
}

// Parts are keyed by text, never by dictionary key: the dictionary can be
// permuted between batches, hold one text twice, be far larger than the
// batch (the sparse lookup) or belong to a larger batch the batch is a
// slice of. The text "a" keeps one group throughout.
#[test]
fn dictionary_changes_between_batches_match_reference() {
    let schema = schema_of(vec![
        field("job", DataType::Utf8),
        field("id", DataType::Utf8),
    ]);
    let jobs = |rows: usize| texts(&["x"], &vec![Some(0); rows]);
    let ids = |entries: &[&str], keys: &[u32]| {
        texts(entries, &keys.iter().map(|&k| Some(k)).collect::<Vec<_>>())
    };
    let larger = batch_of(
        &schema,
        vec![
            jobs(10),
            ids(&["c", "d", "a", "b", "e"], &[0, 1, 2, 3, 4, 2, 0, 1, 3, 4]),
        ],
    );
    let many = (0..10_000)
        .map(|i| {
            if i == 17 {
                "a".to_string()
            } else {
                format!("s{i}")
            }
        })
        .collect::<Vec<_>>();
    let many = many.iter().map(String::as_str).collect::<Vec<_>>();
    let batches = [
        batch_of(&schema, vec![jobs(3), ids(&["a", "b"], &[0, 1, 0])]),
        batch_of(&schema, vec![jobs(4), ids(&["b", "a", "c"], &[1, 0, 2, 1])]),
        batch_of(&schema, vec![jobs(4), ids(&["a", "a"], &[0, 1, 1, 0])]),
        larger.slice(3, 4),
        batch_of(&schema, vec![jobs(3), ids(&many, &[9_999, 17, 5_000])]),
    ];
    assert!(
        !arrow_columns::dense_dictionary(10_000, 3),
        "the sparse path"
    );
    assert_same_as_reference(&[0, 1], &batches, "dictionaries");
    let (table, group_of_row) = assert_same_as_reference(&[1], &batches, "dictionaries");
    let texts_of_rows = batches
        .iter()
        .flat_map(|batch| batch.to_rows())
        .map(|row| row[1].clone())
        .collect::<Vec<_>>();
    let groups_of_a = texts_of_rows
        .iter()
        .zip(&group_of_row)
        .filter(|(text, _)| matches!(text, Value::Utf8(t) if &**t == "a"))
        .map(|(_, &group)| group)
        .collect::<HashSet<_>>();
    assert_eq!(groups_of_a.len(), 1, "{groups_of_a:?}");
    assert_eq!(table.len(), 6, "a, b, c, e, s9999 and s5000");
}

// Reuse of the previous row's group never crosses batches: batch 2's first
// row has the same dictionary key (0), job and number as batch 1's last row,
// but key 0 now means "b", a new group.
#[test]
fn previous_row_state_resets_per_batch() {
    let schema = schema_of(vec![
        field("id", DataType::Utf8),
        field("job", DataType::Utf8),
        field("n", DataType::Int64),
    ]);
    let batch = |entries: &[&str], keys: [u32; 2]| {
        batch_of(
            &schema,
            vec![
                texts(entries, &keys.map(Some)),
                texts(&["x"], &[Some(0), Some(0)]),
                plain(&DataType::Int64, &[Value::Int64(7), Value::Int64(7)]),
            ],
        )
    };
    // Rows "c", "a" (key 0 last), then "b" (key 0 first), "c".
    let batches = [batch(&["a", "c"], [1, 0]), batch(&["b", "c"], [0, 1])];
    for columns in [vec![0], vec![0, 1], vec![2, 0]] {
        let (table, ids) = assert_same_as_reference(&columns, &batches, "previous row");
        assert_eq!(ids, [0, 1, 2, 0], "{columns:?}: c, a | b, c");
        assert_eq!(table.len(), 3, "{columns:?}");
    }
}

// The lead column (the series identity) need not determine the other group
// columns: "s1" comes with job "x", then "y", then NULL. Each is its own
// group, whichever column comes first.
#[test]
fn identity_that_stops_determining_its_labels_is_checked() {
    let schema = schema_of(vec![
        field("job", DataType::Utf8),
        field("id", DataType::Utf8),
    ]);
    let batch = |rows: &[(Option<u32>, u32)]| {
        let jobs = rows.iter().map(|row| row.0).collect::<Vec<_>>();
        let ids = rows.iter().map(|row| Some(row.1)).collect::<Vec<_>>();
        batch_of(
            &schema,
            vec![texts(&["x", "y"], &jobs), texts(&["s1", "s2", "s3"], &ids)],
        )
    };
    let mut first = vec![(Some(0), 0); 10];
    first.extend([(Some(1), 0), (Some(0), 1), (Some(0), 2)]);
    let second = [(None, 0), (Some(0), 0), (Some(1), 0), (Some(0), 1)];
    let batches = [batch(&first), batch(&second)];
    for columns in [vec![0, 1], vec![1, 0]] {
        let (table, ids) = assert_same_as_reference(&columns, &batches, "identity");
        assert_eq!(
            table.len(),
            5,
            "{columns:?}: (x|y|NULL, s1), (x, s2), (x, s3)"
        );
        assert!(ids[..10].iter().all(|&id| id == ids[0]), "{columns:?}");
        let (x_s1, y_s1, null_s1) = (ids[0], ids[10], ids[13]);
        assert!(
            x_s1 != y_s1 && null_s1 != x_s1 && null_s1 != y_s1,
            "{ids:?}"
        );
        assert_eq!(&ids[14..], [x_s1, y_s1, ids[11]], "{columns:?}");
    }
}

// Runs of one key, alternations, and a run equal by key but not by bits,
// over 3,000 rows: whole and in 1,024-row chunks. Reusing the previous row
// must use key equality on every group column.
#[test]
fn runs_and_alternations_match_reference() {
    let schema = schema_of(vec![
        field("f", DataType::Float64),
        field("id", DataType::Utf8),
        field("n", DataType::Int64),
        field("m", DataType::Int64),
    ]);
    let entries = (0..10).map(|i| format!("s{i}")).collect::<Vec<_>>();
    let entries = entries.iter().map(String::as_str).collect::<Vec<_>>();
    let equal_by_key = [
        0.0,
        -0.0,
        float_bits(0x7ff8_0000_0000_0001),
        float_bits(0xfff8_0000_0000_0000),
        f64::NAN,
    ];
    let (mut floats, mut keys, mut ns, mut ms) = (vec![], vec![], vec![], vec![]);
    for row in 0..3_000usize {
        let (f, id, n, m) = match row {
            // Runs of 100 rows of one key.
            0..1_000 => {
                let run = row / 100;
                (run as f64, run % 10, run as i64, (run % 3) as i64)
            }
            // A B A B, with (n, m) = (1, 2) then (2, 1).
            1_000..2_000 if row.is_multiple_of(2) => (-0.0, 0, 1, 2),
            1_000..2_000 => (1.0, 1, 2, 1),
            // One key; the float's bits change every row.
            _ => (equal_by_key[row % 5], 3, 5, 5),
        };
        floats.push(Value::Float64(f));
        keys.push(Some(id as u32));
        ns.push(Value::Int64(n));
        ms.push(Value::Int64(m));
    }
    let batch = batch_of(
        &schema,
        vec![
            plain(&DataType::Float64, &floats),
            texts(&entries, &keys),
            plain(&DataType::Int64, &ns),
            plain(&DataType::Int64, &ms),
        ],
    );
    let whole = [batch];
    let chunks = chunked(&whole);
    assert_eq!(chunks.len(), 3);
    for columns in [vec![0], vec![1, 0], vec![2, 3]] {
        assert_same_as_reference(&columns, &whole, "runs, whole");
        let (_, ids) = assert_same_as_reference(&columns, &chunks, "runs, chunked");
        assert_ne!(ids[1_000], ids[1_001], "{columns:?}");
        assert_eq!(ids[1_000], ids[1_002], "{columns:?}");
    }
    let (table, ids) = assert_same_as_reference(&[0], &whole, "runs");
    assert!(
        ids[2_000..].iter().step_by(5).all(|&id| id == ids[0]),
        "±0 is group 0"
    );
    assert!(ids[2_002..]
        .chunks(5)
        .all(|nans| nans[..3].iter().all(|&id| id == ids[2_002])));
    assert!(
        same_bits(table.label(ids[0], 0), &Value::Float64(0.0)),
        "row 0's 0.0"
    );
    assert!(same_bits(
        table.label(ids[2_002], 0),
        &Value::Float64(float_bits(0x7ff8_0000_0000_0001))
    ));
}

// Many groups and many parts: 200,000 integers over 3 batches, 100,000
// series identities in 1,024-row chunks of one batch (the sparse lookup),
// and (user id, service) pairs in random order.
#[test]
fn high_cardinality_matches_reference() {
    let ints = schema_of(vec![field("n", DataType::Int64)]);
    let int_batches = (0..3)
        .map(|part| {
            let values = (part * 66_667..((part + 1) * 66_667).min(200_000))
                .map(|i| Value::Int64((i * 7_919 % 200_000) as i64))
                .collect::<Vec<_>>();
            batch_of(&ints, vec![plain(&DataType::Int64, &values)])
        })
        .collect::<Vec<_>>();
    let (table, _) = assert_same_as_reference(&[0], &int_batches, "integers");
    assert_eq!(table.len(), 200_000);

    let identities = schema_of(vec![field("id", DataType::Utf8)]);
    let entries = (0..100_000)
        .map(|i| format!("{{instance=\"host-{i:06}\"}}"))
        .collect::<Vec<_>>();
    let entries = entries.iter().map(String::as_str).collect::<Vec<_>>();
    let keys = (0..100_000u32)
        .map(|i| Some(i * 7_919 % 100_000))
        .collect::<Vec<_>>();
    let one_batch = [batch_of(&identities, vec![texts(&entries, &keys)])];
    assert!(!arrow_columns::dense_dictionary(100_000, ROWS_PER_CHUNK));
    let (table, _) = assert_same_as_reference(&[0], &chunked(&one_batch), "identities");
    assert_eq!(table.len(), 100_000);

    let pairs = schema_of(vec![
        field("user", DataType::Float64),
        field("service", DataType::Utf8),
    ]);
    let services = (0..10).map(|i| format!("svc{i}")).collect::<Vec<_>>();
    let services = services.iter().map(String::as_str).collect::<Vec<_>>();
    let mut rng = Lcg(9);
    let mut distinct = HashSet::new();
    let pair_batches = (0..2)
        .map(|_| {
            let (mut users, mut keys) = (vec![], vec![]);
            for _ in 0..100_000 {
                let (user, service) = (rng.below(50_000) as f64 + 0.5, rng.below(10) as u32);
                distinct.insert((user.to_bits(), service));
                users.push(Value::Float64(user));
                keys.push(Some(service));
            }
            batch_of(
                &pairs,
                vec![plain(&DataType::Float64, &users), texts(&services, &keys)],
            )
        })
        .collect::<Vec<_>>();
    let (table, _) = assert_same_as_reference(&[0, 1], &chunked(&pair_batches), "pairs");
    assert_eq!(table.len(), distinct.len());
}

// Every combination of text (dictionary) and primitive columns: several
// dictionaries of different sizes, a decoded type (Date), one column twice,
// and a largest dictionary (job, 50 entries) that does not determine the
// others while a smaller one (id, 20 entries) determines it.
#[test]
fn mixed_dictionary_and_primitive_keys_match_reference() {
    let schema = schema_of(vec![
        field("a", DataType::Utf8),
        field("n", DataType::Int64),
        field("f", DataType::Float64),
        field("b", DataType::Utf8),
        field("time", DataType::Timestamp),
        field("c", DataType::Utf8),
        field("flag", DataType::Bool),
        field("day", DataType::Date),
        field("job", DataType::Utf8),
        field("id", DataType::Utf8),
    ]);
    let entries = |prefix: &str, count: usize| {
        (0..count)
            .map(|i| format!("{prefix}{i}"))
            .collect::<Vec<_>>()
    };
    let (a, b, c, job, id) = (
        entries("a", 5),
        entries("b", 30),
        entries("c", 12),
        entries("job", 50),
        entries("id", 20),
    );
    let mut rng = Lcg(5);
    let batches = (0..2)
        .map(|_| {
            const ROWS: usize = 3_000;
            let keys = |rng: &mut Lcg, count: usize| {
                (0..ROWS)
                    .map(|_| (rng.below(20) != 0).then(|| rng.below(count) as u32))
                    .collect::<Vec<_>>()
            };
            let ids = (0..ROWS).map(|_| rng.below(20) as u32).collect::<Vec<_>>();
            let column = |rng: &mut Lcg, dtype: DataType| random_column(rng, &dtype, true, ROWS);
            batch_of(
                &schema,
                vec![
                    texts(&refs(&a), &keys(&mut rng, a.len())),
                    column(&mut rng, DataType::Int64),
                    column(&mut rng, DataType::Float64),
                    texts(&refs(&b), &keys(&mut rng, b.len())),
                    column(&mut rng, DataType::Timestamp),
                    texts(&refs(&c), &keys(&mut rng, c.len())),
                    column(&mut rng, DataType::Bool),
                    column(&mut rng, DataType::Date),
                    texts(
                        &refs(&job),
                        &ids.iter().map(|&id| Some(id % 7)).collect::<Vec<_>>(),
                    ),
                    texts(
                        &refs(&id),
                        &ids.iter().map(|&id| Some(id)).collect::<Vec<_>>(),
                    ),
                ],
            )
        })
        .collect::<Vec<_>>();
    for columns in [
        vec![0, 1],
        vec![2, 3, 4],
        vec![0, 3, 5],
        vec![5, 3, 0],
        vec![6, 7],
        vec![7],
        vec![1, 1],
        vec![3, 3],
        vec![8, 9],
        vec![9, 8],
        vec![8, 9, 2],
    ] {
        assert_same_as_reference(&columns, &batches, "mixed");
        assert_same_as_reference(&columns, &chunked(&batches), "mixed, chunked");
    }
}

// A differential property test: random column types, orders, nullability,
// batch sizes, dictionaries (shuffled, duplicate texts, unused entries) and
// slices, whole and through 1,024-row chunks, all as the reference.
#[test]
fn random_batches_match_reference() {
    const TYPES: [DataType; 6] = [
        DataType::Utf8,
        DataType::Float64,
        DataType::Int64,
        DataType::Timestamp,
        DataType::Bool,
        DataType::Date,
    ];
    for case in 0..500u64 {
        let seed = case * 7_919 + 1;
        let mut rng = Lcg(seed);
        let width = 1 + rng.below(4);
        let fields = (0..width)
            .map(|index| {
                let mut column = field(&format!("c{index}"), TYPES[rng.below(TYPES.len())].clone());
                column.nullable = rng.below(2) == 0;
                column
            })
            .collect::<Vec<_>>();
        let schema = schema_of(fields);
        let mut columns = (0..width).collect::<Vec<_>>();
        rng.shuffle(&mut columns);
        let batches = (0..1 + rng.below(5))
            .map(|_| {
                let rows = rng.below(2_101);
                let (offset, extra) = if rng.below(4) == 0 {
                    (rng.below(50), rng.below(50))
                } else {
                    (0, 0)
                };
                let arrays = schema
                    .fields
                    .iter()
                    .map(|field| {
                        let FieldDataType::Plain(dtype) = &field.dtype else {
                            unreachable!()
                        };
                        random_column(&mut rng, dtype, field.nullable, offset + rows + extra)
                    })
                    .collect();
                batch_of(&schema, arrays).slice(offset, rows)
            })
            .collect::<Vec<_>>();
        let context = format!("case {case} (seed {seed})");
        assert_same_as_reference(&columns, &batches, &context);
        assert_same_as_reference(&columns, &chunked(&batches), &format!("{context}, chunked"));
    }
}

// Callers charge the difference of `memory_bytes` before and after: it
// rises with every new part, never falls, stays put when only known values
// arrive, and covers at least what each new value really holds.
#[test]
fn memory_bytes_grows_and_charges_typed_parts() {
    let schema = schema_of(vec![
        field("f", DataType::Float64),
        field("n", DataType::Int64),
    ]);
    let batches = (0..10)
        .map(|part| {
            let range = part * 1_000..(part + 1) * 1_000;
            let floats = range
                .clone()
                .map(|i| Value::Float64(i as f64 + 0.25))
                .collect::<Vec<_>>();
            let ints = range.map(|i| Value::Int64(i * 3)).collect::<Vec<_>>();
            batch_of(
                &schema,
                vec![
                    plain(&DataType::Float64, &floats),
                    plain(&DataType::Int64, &ints),
                ],
            )
        })
        .collect::<Vec<_>>();
    for columns in [vec![0], vec![1], vec![0, 1]] {
        let mut table = GroupTable::new(columns.clone());
        let mut memory_bytes = table.memory_bytes();
        for batch in &batches {
            table.group_ids(batch).unwrap();
            assert!(table.memory_bytes() > memory_bytes, "{columns:?}");
            memory_bytes = table.memory_bytes();
        }
        for batch in &batches {
            table.group_ids(batch).unwrap();
            assert_eq!(table.memory_bytes(), memory_bytes, "{columns:?}");
        }
        assert_eq!(table.len(), 10_000);
        // Held per new value at least: its first value, its 8-byte key and
        // 4-byte part id in a map, and its group's part tuple (one `u32`
        // per column behind a 16-byte boxed slice).
        let held = columns.len() * (std::mem::size_of::<Value>() + 12) + 16 + 4 * columns.len();
        assert!(
            memory_bytes >= 10_000 * held,
            "{columns:?}: {memory_bytes} bytes for 10,000 values"
        );
    }
}

/// One measured input shape of the benchmark below.
struct Shape {
    name: &'static str,
    /// The input's group columns.
    columns: Vec<usize>,
    /// The input, fed through `row_chunks` as grouped operators feed it.
    batches: Vec<Batch>,
    /// The median ns per row to reach; `None` asks only for no slowdown.
    target_ns_per_row: Option<f64>,
    /// The largest median ns per row of the new table over the reference's.
    max_new_over_reference: f64,
}

impl Shape {
    fn rows(&self) -> usize {
        self.batches.iter().map(Batch::num_rows).sum()
    }
}

/// Series identities as PromQL forms them, with the job each carries.
fn series_texts(series: usize) -> (ArrayRef, ArrayRef) {
    let identities = (0..series)
        .map(|s| {
            format!(
                "{{__name__=\"latency\", instance=\"host-{s:05}\", job=\"job-{}\"}}",
                s % 10
            )
        })
        .collect::<Vec<_>>();
    let jobs = (0..10).map(|j| format!("job-{j}")).collect::<Vec<_>>();
    (
        Arc::new(StringArray::from(identities)),
        Arc::new(StringArray::from(jobs)),
    )
}

/// A (job, identity) batch whose rows are the series `series_of_rows`.
fn series_batch(
    schema: &SchemaRef,
    identities: &ArrayRef,
    jobs: &ArrayRef,
    series_of_rows: impl Iterator<Item = usize>,
) -> Batch {
    let series = series_of_rows.collect::<Vec<_>>();
    batch_of(
        schema,
        vec![
            texts_over(
                jobs.clone(),
                &series
                    .iter()
                    .map(|&s| Some((s % 10) as u32))
                    .collect::<Vec<_>>(),
            ),
            texts_over(
                identities.clone(),
                &series.iter().map(|&s| Some(s as u32)).collect::<Vec<_>>(),
            ),
        ],
    )
}

/// The shapes of the executor profile's group-id hot spots.
fn benchmark_shapes() -> Vec<Shape> {
    let series_schema = schema_of(vec![
        field("job", DataType::Utf8),
        field("id", DataType::Utf8),
    ]);
    let (identities, jobs) = series_texts(1_000);
    // An exact scan over an hour at 1 Hz, read series by series.
    let scan = (0..100)
        .map(|part| {
            series_batch(
                &series_schema,
                &identities,
                &jobs,
                (part * 10..(part + 1) * 10).flat_map(|s| std::iter::repeat_n(s, 3_600)),
            )
        })
        .collect::<Vec<_>>();
    // Ingestion: one sample per series per batch.
    let ingestion = (0..360)
        .map(|_| series_batch(&series_schema, &identities, &jobs, 0..1_000))
        .collect::<Vec<_>>();
    let (many_identities, many_jobs) = series_texts(10_000);
    let ingestion_10k = (0..36)
        .map(|_| series_batch(&series_schema, &many_identities, &many_jobs, 0..10_000))
        .collect::<Vec<_>>();
    // Merges: one stored window per batch, each with its own dictionary.
    let windows = (0..360)
        .map(|window| {
            let (identities, jobs) = series_texts(1_000);
            series_batch(
                &series_schema,
                &identities,
                &jobs,
                (0..1_000).map(|s| (s + window * 37) % 1_000),
            )
        })
        .collect::<Vec<_>>();
    // SQL keys, in random order, in 8,192-row source batches.
    let mut rng = Lcg(11);
    const SQL_BATCH_ROWS: usize = 8_192;
    let users = schema_of(vec![field("user_id", DataType::Float64)]);
    let user_ids = |rng: &mut Lcg, values: usize, rows: usize| {
        (0..rows.div_ceil(SQL_BATCH_ROWS))
            .map(|part| {
                let ids = (0..SQL_BATCH_ROWS.min(rows - part * SQL_BATCH_ROWS))
                    .map(|_| Value::Float64(1e6 + rng.below(values) as f64))
                    .collect::<Vec<_>>();
                batch_of(&users, vec![plain(&DataType::Float64, &ids)])
            })
            .collect::<Vec<_>>()
    };
    let distinct_1m = user_ids(&mut rng, 1_000_000, 3_000_000);
    let distinct_10k = user_ids(&mut rng, 10_000, 3_000_000);
    let regions = schema_of(vec![
        field("region", DataType::Utf8),
        field("service", DataType::Utf8),
    ]);
    let region_texts: ArrayRef = Arc::new(StringArray::from(
        (0..20).map(|i| format!("region-{i}")).collect::<Vec<_>>(),
    ));
    let service_texts: ArrayRef = Arc::new(StringArray::from(
        (0..50).map(|i| format!("service-{i}")).collect::<Vec<_>>(),
    ));
    let region_service = (0..1_000_000usize.div_ceil(SQL_BATCH_ROWS))
        .map(|part| {
            let rows = SQL_BATCH_ROWS.min(1_000_000 - part * SQL_BATCH_ROWS);
            let groups = (0..rows).map(|_| rng.below(1_000)).collect::<Vec<_>>();
            batch_of(
                &regions,
                vec![
                    texts_over(
                        region_texts.clone(),
                        &groups
                            .iter()
                            .map(|&g| Some((g / 50) as u32))
                            .collect::<Vec<_>>(),
                    ),
                    texts_over(
                        service_texts.clone(),
                        &groups
                            .iter()
                            .map(|&g| Some((g % 50) as u32))
                            .collect::<Vec<_>>(),
                    ),
                ],
            )
        })
        .collect::<Vec<_>>();
    let primitives = schema_of(vec![
        field("n", DataType::Int64),
        field("f", DataType::Float64),
        field("time", DataType::Timestamp),
    ]);
    let primitive_tuples = (0..1_000_000usize.div_ceil(SQL_BATCH_ROWS))
        .map(|part| {
            let rows = SQL_BATCH_ROWS.min(1_000_000 - part * SQL_BATCH_ROWS);
            let groups = (0..rows).map(|_| rng.below(10_000)).collect::<Vec<_>>();
            let column = |dtype: DataType, value: fn(usize) -> Value| {
                plain(
                    &dtype,
                    &groups.iter().map(|&g| value(g)).collect::<Vec<_>>(),
                )
            };
            batch_of(
                &primitives,
                vec![
                    column(DataType::Int64, |g| Value::Int64(g as i64)),
                    column(DataType::Float64, |g| Value::Float64(g as f64 * 0.5)),
                    column(DataType::Timestamp, |g| Value::Timestamp(g as i64 * 1_000)),
                ],
            )
        })
        .collect::<Vec<_>>();
    let shape = |name, columns, batches, target: Option<f64>, max_ratio| Shape {
        name,
        columns,
        batches,
        target_ns_per_row: target,
        max_new_over_reference: max_ratio,
    };
    vec![
        shape(
            "S1 exact scan, series runs",
            vec![0, 1],
            scan.clone(),
            Some(8.0),
            1.0,
        ),
        shape(
            "S2 ingestion, 1,000 series",
            vec![0, 1],
            ingestion,
            Some(15.0),
            1.0,
        ),
        shape(
            "S3 ingestion, 10,000 series",
            vec![0, 1],
            ingestion_10k,
            Some(20.0),
            1.0,
        ),
        shape(
            "S4 merge of stored windows",
            vec![0, 1],
            windows,
            Some(15.0),
            1.0,
        ),
        shape("S5 single identity", vec![1], scan, None, 1.05),
        shape(
            "S6 SQL distinct, 1 M values",
            vec![0],
            distinct_1m,
            Some(40.0),
            1.0,
        ),
        shape(
            "S6 SQL distinct, 10,000 values",
            vec![0],
            distinct_10k,
            Some(15.0),
            1.0,
        ),
        shape(
            "S7 two dictionaries",
            vec![0, 1],
            region_service,
            Some(12.0),
            1.0,
        ),
        shape(
            "S8 primitive tuples",
            vec![0, 1, 2],
            primitive_tuples,
            Some(40.0),
            1.0,
        ),
    ]
}

/// The ns per row of one run of a fresh table over `shape`'s input.
fn ns_per_row<T>(
    shape: &Shape,
    new: impl FnOnce(Vec<usize>) -> T,
    group_ids: impl Fn(&mut T, &Batch) -> Vec<u32>,
) -> f64 {
    let start = std::time::Instant::now();
    let mut table = new(shape.columns.clone());
    for batch in &shape.batches {
        for chunk in row_chunks(batch) {
            std::hint::black_box(group_ids(&mut table, &chunk));
        }
    }
    let elapsed = start.elapsed();
    drop(table);
    elapsed.as_nanos() as f64 / shape.rows() as f64
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

// Group ids alone, ns per row, for the reference and the new table, timed
// alternately in one process: one warm-up, then 11 runs each. Checks every
// shape's median against its target and against the reference. Run alone,
// pinned to one core of an idle host:
//   taskset -c <core> cargo test -p asap-executor --release --lib \
//     group_table::tests::group_ids_ns_per_row -- --ignored --nocapture
#[test]
#[ignore = "wall-clock benchmark; run by hand on an idle host"]
fn group_ids_ns_per_row() {
    const RUNS: usize = 11;
    let shapes = benchmark_shapes();
    println!(
        "| Shape | Rows | Reference median (min) | New median (min) | New ÷ reference | Target | Met |"
    );
    println!("|---|---|---|---|---|---|---|");
    let mut missed = Vec::new();
    for shape in &shapes {
        let (mut reference, mut new) = (Vec::new(), Vec::new());
        for run in 0..=RUNS {
            let old = ns_per_row(shape, ReferenceGroupTable::new, |table, batch| {
                table.group_ids(batch).unwrap()
            });
            let current = ns_per_row(shape, GroupTable::new, |table, batch| {
                table.group_ids(batch).unwrap()
            });
            if run > 0 {
                reference.push(old);
                new.push(current);
            }
        }
        let min = |values: &[f64]| values.iter().copied().fold(f64::INFINITY, f64::min);
        let (reference_min, new_min) = (min(&reference), min(&new));
        let (reference_median, new_median) = (median(&mut reference), median(&mut new));
        let ratio = new_median / reference_median;
        let met = shape
            .target_ns_per_row
            .is_none_or(|target| new_median <= target)
            && ratio <= shape.max_new_over_reference;
        if !met {
            missed.push(shape.name);
        }
        let target = match shape.target_ns_per_row {
            Some(target) => format!("≤ {target} ns, ≤ {:.2}×", shape.max_new_over_reference),
            None => format!("≤ {:.2}×", shape.max_new_over_reference),
        };
        println!(
            "| {} | {} | {reference_median:.1} ({reference_min:.1}) | {new_median:.1} ({new_min:.1}) | {ratio:.3} | {target} | {} |",
            shape.name,
            shape.rows(),
            if met { "yes" } else { "no" }
        );
    }
    assert!(missed.is_empty(), "targets missed: {missed:?}");
}
