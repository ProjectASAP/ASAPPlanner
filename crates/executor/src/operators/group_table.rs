//! Group ids for grouped operators: each row's group as a `u32`, assigned
//! across all batches of one operator run.
//!
//! A group is the tuple of its group columns' values, with the equality of
//! [`Value::key`] (zero signs and NaN payloads form one group, NULL is a
//! group). Each column's distinct values get a `u32` part id; a group is the
//! tuple of its part ids. A dictionary-encoded text column (a PromQL label
//! or series identity) is resolved once per distinct dictionary entry per
//! batch, so a series' group is found once per batch, not once per row.
//! A group is labelled with its own first row's values, as the row
//! operators labelled it.
use crate::values::arrow_columns::{self, ColumnDepth};
use crate::values::{Batch, Value};
use crate::Error;
use ahash::AHashMap as HashMap;
use arrow::array::{
    Array, ArrayRef, BooleanArray, DictionaryArray, Float64Array, Int64Array, StringArray,
    TimestampMillisecondArray,
};
use arrow::datatypes::UInt32Type;
use planner_types::ir::schema::{DataType, FieldDataType};

/// Approximate bytes of one hash-map entry beyond its key, for accounting.
const MAP_ENTRY_BYTES: usize = 32;

fn too_many(what: &str) -> Error {
    Error::Invalid(format!("more than u32::MAX {what}"))
}

/// The distinct values of one group column, by part id.
#[derive(Default)]
struct ColumnParts {
    /// Part id by the value's [`Value::key`] bytes.
    part_of_key: HashMap<Box<[u8]>, u32>,
    /// The first value seen of each part id.
    first_values: Vec<Value>,
    /// Memory the parts hold, in bytes.
    memory_bytes: usize,
}

impl ColumnParts {
    /// The part id of the value with key bytes `key`; `value` makes it the
    /// first time.
    fn part(&mut self, key: &[u8], value: impl FnOnce() -> Value) -> Result<u32, Error> {
        if let Some(&id) = self.part_of_key.get(key) {
            return Ok(id);
        }
        let id = u32::try_from(self.first_values.len()).map_err(|_| too_many("group values"))?;
        let value = value();
        self.memory_bytes += key.len() + MAP_ENTRY_BYTES + value.bytes();
        self.part_of_key.insert(key.into(), id);
        self.first_values.push(value);
        Ok(id)
    }

    /// Each part id's position in key order.
    fn ranks(&self) -> Vec<u32> {
        let mut by_key = self.part_of_key.iter().collect::<Vec<_>>();
        by_key.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let mut rank = vec![0u32; by_key.len()];
        for (position, (_, &part)) in by_key.into_iter().enumerate() {
            rank[part as usize] = position as u32;
        }
        rank
    }
}

/// Group numbers for the rows of one grouped operator's input.
pub(crate) struct GroupTable {
    /// The input's group columns.
    group_columns: Vec<usize>,
    /// Per group column, its distinct values.
    parts: Vec<ColumnParts>,
    /// With one group column: the group of each part id.
    group_of_part: Vec<Option<u32>>,
    /// With several group columns: the group of each tuple of part ids.
    group_of_part_tuple: HashMap<Box<[u32]>, u32>,
    /// Per group, its part ids, one per group column.
    parts_of_group: Vec<Box<[u32]>>,
    /// With several group columns: each group's own first values, its
    /// labels (with one column, a part's first value is its group's).
    labels_of_group: Vec<Box<[Value]>>,
    /// Memory the groups hold beyond their columns' parts, in bytes.
    group_memory_bytes: usize,
}

/// Value::key bytes of a non-null `Float64`, written into `out`.
fn float_key(value: f64, out: &mut Vec<u8>) {
    let bits = if value == 0. {
        0
    } else if value.is_nan() {
        f64::NAN.to_bits()
    } else {
        value.to_bits()
    };
    out.push(3);
    out.extend_from_slice(&bits.to_le_bytes());
}

/// The value of `batch`'s plain column `column` at `row`.
fn value_at(batch: &Batch, column: usize, row: usize) -> Value {
    let FieldDataType::Plain(dtype) = &batch.schema().fields[column].dtype else {
        unreachable!("group columns are plain")
    };
    arrow_columns::decode(
        batch.column(column).slice(row, 1).as_ref(),
        dtype,
        ColumnDepth::BatchColumn,
    )
    .remove(0)
}

impl GroupTable {
    /// A table over the input's `group_columns`; with none, every row is in
    /// one group.
    pub(crate) fn new(group_columns: Vec<usize>) -> Self {
        Self {
            parts: group_columns
                .iter()
                .map(|_| ColumnParts::default())
                .collect(),
            group_columns,
            group_of_part: Vec::new(),
            group_of_part_tuple: HashMap::new(),
            parts_of_group: Vec::new(),
            labels_of_group: Vec::new(),
            group_memory_bytes: 0,
        }
    }

    /// The number of groups so far.
    pub(crate) fn len(&self) -> usize {
        self.parts_of_group.len()
    }

    /// Memory the table holds (values, keys, lookups and labels), in bytes.
    pub(crate) fn memory_bytes(&self) -> usize {
        self.group_memory_bytes
            + self
                .parts
                .iter()
                .map(|column| column.memory_bytes)
                .sum::<usize>()
    }

    /// Add a group with no group column (the one global group).
    pub(crate) fn add_empty_group(&mut self) -> Result<u32, Error> {
        debug_assert!(self.group_columns.is_empty());
        self.new_group(Box::new([]), None)
    }

    fn new_group(&mut self, parts: Box<[u32]>, labels: Option<Box<[Value]>>) -> Result<u32, Error> {
        let group = u32::try_from(self.parts_of_group.len()).map_err(|_| too_many("groups"))?;
        self.group_memory_bytes += parts.len() * std::mem::size_of::<u32>() * 2
            + MAP_ENTRY_BYTES
            + labels
                .iter()
                .flat_map(|labels| labels.iter())
                .map(Value::bytes)
                .sum::<usize>();
        if self.group_columns.len() == 1 {
            let part = parts[0] as usize;
            if self.group_of_part.len() <= part {
                self.group_of_part.resize(part + 1, None);
            }
            self.group_of_part[part] = Some(group);
        } else {
            self.group_of_part_tuple.insert(parts.clone(), group);
        }
        if let Some(labels) = labels {
            self.labels_of_group.push(labels);
        }
        self.parts_of_group.push(parts);
        Ok(group)
    }

    /// The part id of each row's value in group column `index`.
    fn column_parts(&mut self, batch: &Batch, index: usize) -> Result<Vec<u32>, Error> {
        let column = self.group_columns[index];
        let array = batch.column(column);
        let parts = &mut self.parts[index];
        let rows = batch.num_rows();
        let mut key = Vec::with_capacity(16);
        if let Some(dictionary) = array.as_any().downcast_ref::<DictionaryArray<UInt32Type>>() {
            // One lookup per distinct dictionary entry the batch uses: a
            // dense slot per entry, or a map when the dictionary is much
            // larger than the batch (a slice of a large batch).
            let texts = dictionary
                .values()
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("batch text columns are dictionaries over strings");
            let keys = dictionary.keys();
            let dense = texts.len() <= 4 * rows + 64;
            let mut slot_of_entry: Vec<Option<u32>> = if dense {
                vec![None; texts.len()]
            } else {
                Vec::new()
            };
            let mut map_of_entry: HashMap<u32, u32> = HashMap::new();
            let mut null_part = None;
            let mut lookup =
                |entry: u32, parts: &mut ColumnParts, key: &mut Vec<u8>| -> Result<u32, Error> {
                    let cached = if dense {
                        slot_of_entry[entry as usize]
                    } else {
                        map_of_entry.get(&entry).copied()
                    };
                    if let Some(part) = cached {
                        return Ok(part);
                    }
                    let text = texts.value(entry as usize);
                    key.clear();
                    key.push(4);
                    key.extend_from_slice(text.as_bytes());
                    let part = parts.part(key, || Value::Utf8(text.into()))?;
                    if dense {
                        slot_of_entry[entry as usize] = Some(part);
                    } else {
                        map_of_entry.insert(entry, part);
                    }
                    Ok(part)
                };
            return (0..rows)
                .map(|row| {
                    if keys.is_null(row) {
                        if let Some(part) = null_part {
                            return Ok(part);
                        }
                        let part = parts.part(&[0], || Value::Null)?;
                        null_part = Some(part);
                        Ok(part)
                    } else {
                        lookup(keys.value(row), parts, &mut key)
                    }
                })
                .collect();
        }
        let any = array.as_any();
        macro_rules! primitive {
            ($array:ty, $write:expr, $value:expr) => {{
                let typed = any
                    .downcast_ref::<$array>()
                    .expect("declared column layout");
                return (0..rows)
                    .map(|row| {
                        key.clear();
                        if typed.is_null(row) {
                            key.push(0);
                            parts.part(&key, || Value::Null)
                        } else {
                            let v = typed.value(row);
                            #[allow(clippy::redundant_closure_call)]
                            ($write)(v, &mut key);
                            #[allow(clippy::redundant_closure_call)]
                            parts.part(&key, || ($value)(v))
                        }
                    })
                    .collect();
            }};
        }
        let FieldDataType::Plain(dtype) = &batch.schema().fields[column].dtype else {
            return Err(Error::Invalid(
                "summary states cannot be grouping keys".into(),
            ));
        };
        match dtype {
            DataType::Int64 => primitive!(
                Int64Array,
                |v: i64, key: &mut Vec<u8>| {
                    key.push(2);
                    key.extend_from_slice(&v.to_le_bytes())
                },
                Value::Int64
            ),
            DataType::Timestamp => primitive!(
                TimestampMillisecondArray,
                |v: i64, key: &mut Vec<u8>| {
                    key.push(5);
                    key.extend_from_slice(&v.to_le_bytes())
                },
                Value::Timestamp
            ),
            DataType::Float64 => primitive!(Float64Array, float_key, Value::Float64),
            DataType::Bool => primitive!(
                BooleanArray,
                |v: bool, key: &mut Vec<u8>| key.extend([1, v as u8]),
                Value::Bool
            ),
            _ => {
                // Other types: through their decoded values.
                let values = arrow_columns::decode(array.as_ref(), dtype, ColumnDepth::BatchColumn);
                values
                    .into_iter()
                    .map(|value| {
                        let key = value.key()?;
                        parts.part(&key, || value)
                    })
                    .collect()
            }
        }
    }

    /// The group of each row of `batch`, adding a group for each new tuple
    /// of group values.
    pub(crate) fn group_ids(&mut self, batch: &Batch) -> Result<Vec<u32>, Error> {
        let rows = batch.num_rows();
        match self.group_columns.len() {
            0 => {
                if rows > 0 && self.parts_of_group.is_empty() {
                    self.new_group(Box::new([]), None)?;
                }
                Ok(vec![0; rows])
            }
            1 => {
                let parts = self.column_parts(batch, 0)?;
                parts
                    .into_iter()
                    .map(
                        |part| match self.group_of_part.get(part as usize).copied().flatten() {
                            Some(group) => Ok(group),
                            None => self.new_group(Box::new([part]), None),
                        },
                    )
                    .collect()
            }
            width => {
                let columns = (0..width)
                    .map(|index| self.column_parts(batch, index))
                    .collect::<Result<Vec<_>, _>>()?;
                let mut key = vec![0u32; width];
                (0..rows)
                    .map(|row| {
                        for (slot, parts) in key.iter_mut().zip(&columns) {
                            *slot = parts[row];
                        }
                        match self.group_of_part_tuple.get(key.as_slice()) {
                            Some(&group) => Ok(group),
                            None => {
                                let labels = self
                                    .group_columns
                                    .iter()
                                    .map(|&column| value_at(batch, column, row))
                                    .collect();
                                self.new_group(key.clone().into_boxed_slice(), Some(labels))
                            }
                        }
                    })
                    .collect()
            }
        }
    }

    /// Group `group`'s label in group column `index`: its first row's value.
    pub(crate) fn label(&self, group: u32, index: usize) -> &Value {
        if let Some(labels) = self.labels_of_group.get(group as usize) {
            return &labels[index];
        }
        let part = self.parts_of_group[group as usize][index];
        &self.parts[index].first_values[part as usize]
    }

    /// The groups in the order of their group values' keys: the order the
    /// row-at-a-time operators emitted groups in (a `BTreeMap` by key).
    /// Each column's parts are ranked once by key; groups sort by rank.
    pub(crate) fn groups_in_key_order(&self) -> Vec<u32> {
        let ranks = self
            .parts
            .iter()
            .map(ColumnParts::ranks)
            .collect::<Vec<_>>();
        let mut groups = (0..self.len() as u32).collect::<Vec<_>>();
        groups.sort_unstable_by(|&a, &b| {
            let (a, b) = (
                &self.parts_of_group[a as usize],
                &self.parts_of_group[b as usize],
            );
            a.iter()
                .zip(b.iter())
                .zip(&ranks)
                .map(|((&a, &b), rank)| rank[a as usize].cmp(&rank[b as usize]))
                .find(|order| order.is_ne())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        groups
    }

    /// The group columns of the groups `order`, in the input's types.
    pub(crate) fn label_arrays(
        &self,
        input: &crate::values::SchemaRef,
        order: &[u32],
    ) -> Result<Vec<ArrayRef>, Error> {
        self.group_columns
            .iter()
            .enumerate()
            .map(|(index, &column)| {
                let FieldDataType::Plain(dtype) = &input.fields[column].dtype else {
                    return Err(Error::Invalid("group columns are plain".into()));
                };
                let values = order
                    .iter()
                    .map(|&group| self.label(group, index))
                    .collect::<Vec<_>>();
                arrow_columns::encode(dtype, ColumnDepth::BatchColumn, &values)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use planner_types::ir::schema::{Field, Schema};
    use std::collections::BTreeMap;
    use std::sync::Arc;

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
            table: None,
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
                for row in batch.rows() {
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
            table: None,
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
}
