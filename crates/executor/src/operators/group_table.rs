//! Group ids for grouped operators: each row's group as a `u32`, assigned
//! across all batches of one operator run.
//!
//! A group is the tuple of its group columns' values, with the equality of
//! [`Value::key`] (zero signs and NaN payloads form one group, NULL is a
//! group). Each column's distinct values get a `u32` part id; a group is the
//! tuple of its part ids. A dictionary-encoded text column (a PromQL label
//! or series identity) is resolved once per distinct dictionary entry per
//! batch, so a series' group is found once per batch, not once per row.
use crate::values::arrow_columns::{self, ColumnDepth};
use crate::values::{Batch, Value};
use crate::Error;
use ahash::AHashMap as HashMap;
use arrow::array::{
    Array, ArrayRef, BooleanArray, DictionaryArray, Float64Array, Int64Array,
    TimestampMillisecondArray,
};
use arrow::datatypes::UInt32Type;
use planner_types::ir::schema::{DataType, FieldDataType};

/// The distinct values of one group column, by part id.
#[derive(Default)]
struct ColumnParts {
    /// Part id by the value's [`Value::key`] bytes.
    ids: HashMap<Vec<u8>, u32>,
    /// The value of each part id.
    values: Vec<Value>,
    /// The key bytes of each part id.
    keys: Vec<Box<[u8]>>,
}

impl ColumnParts {
    /// The part id of the value with key bytes `key`; `value` makes it the
    /// first time.
    fn part(&mut self, key: &[u8], value: impl FnOnce() -> Value) -> u32 {
        if let Some(&id) = self.ids.get(key) {
            return id;
        }
        let id = self.values.len() as u32;
        self.ids.insert(key.to_vec(), id);
        self.values.push(value());
        self.keys.push(key.into());
        id
    }
}

/// Group numbers for the rows of one grouped operator's input.
pub(crate) struct GroupTable {
    /// The input's group columns.
    columns: Vec<usize>,
    /// Per group column, its distinct values.
    parts: Vec<ColumnParts>,
    /// With one group column: the group of each part id.
    group_of_part: Vec<Option<u32>>,
    /// With several group columns: the group of each tuple of part ids.
    group_of_parts: HashMap<Box<[u32]>, u32>,
    /// Per group, its part ids, one per group column.
    group_parts: Vec<Box<[u32]>>,
    /// Memory the table holds beyond its states, in bytes.
    memory_bytes: usize,
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

impl GroupTable {
    /// A table over the input's group `columns`; with none, every row is in
    /// one group.
    pub(crate) fn new(columns: Vec<usize>) -> Self {
        Self {
            parts: columns.iter().map(|_| ColumnParts::default()).collect(),
            columns,
            group_of_part: Vec::new(),
            group_of_parts: HashMap::new(),
            group_parts: Vec::new(),
            memory_bytes: 0,
        }
    }

    /// The number of groups so far.
    pub(crate) fn len(&self) -> usize {
        self.group_parts.len()
    }

    /// Memory the table holds (labels, keys and lookup), in bytes.
    pub(crate) fn memory_bytes(&self) -> usize {
        self.memory_bytes
    }

    /// Add a group with no group column (the one global group).
    pub(crate) fn add_empty_group(&mut self) -> u32 {
        debug_assert!(self.columns.is_empty());
        self.new_group(Box::new([]))
    }

    fn new_group(&mut self, parts: Box<[u32]>) -> u32 {
        let group = self.group_parts.len() as u32;
        let labels = parts
            .iter()
            .zip(&self.parts)
            .map(|(&part, column)| column.values[part as usize].bytes())
            .sum::<usize>();
        self.memory_bytes += labels + parts.len() * 4 + 64;
        if self.columns.len() == 1 {
            let part = parts[0] as usize;
            if self.group_of_part.len() <= part {
                self.group_of_part.resize(part + 1, None);
            }
            self.group_of_part[part] = Some(group);
        } else {
            self.group_of_parts.insert(parts.clone(), group);
        }
        self.group_parts.push(parts);
        group
    }

    /// The part id of each row's value in group column `index`.
    fn column_parts(&mut self, batch: &Batch, index: usize) -> Result<Vec<u32>, Error> {
        let column = self.columns[index];
        let array = batch.column(column);
        let parts = &mut self.parts[index];
        let rows = batch.num_rows();
        let mut key = Vec::with_capacity(16);
        if let Some(dictionary) = array.as_any().downcast_ref::<DictionaryArray<UInt32Type>>() {
            // One lookup per distinct dictionary entry the batch uses.
            let texts = dictionary
                .values()
                .as_any()
                .downcast_ref::<arrow::array::StringArray>()
                .expect("batch text columns are dictionaries over strings");
            let mut part_of_entry: Vec<Option<u32>> = vec![None; texts.len()];
            let keys = dictionary.keys();
            let mut null_part = None;
            return Ok((0..rows)
                .map(|row| {
                    if keys.is_null(row) {
                        *null_part.get_or_insert_with(|| parts.part(&[0], || Value::Null))
                    } else {
                        let entry = keys.value(row) as usize;
                        *part_of_entry[entry].get_or_insert_with(|| {
                            let text = texts.value(entry);
                            key.clear();
                            key.push(4);
                            key.extend_from_slice(text.as_bytes());
                            parts.part(&key, || Value::Utf8(text.into()))
                        })
                    }
                })
                .collect());
        }
        let any = array.as_any();
        macro_rules! primitive {
            ($array:ty, $write:expr, $value:expr) => {{
                let typed = any
                    .downcast_ref::<$array>()
                    .expect("declared column layout");
                return Ok((0..rows)
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
                    .collect());
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
                        Ok(parts.part(&key, || value))
                    })
                    .collect()
            }
        }
    }

    /// The group of each row of `batch`, adding a group for each new tuple
    /// of group values.
    pub(crate) fn group_ids(&mut self, batch: &Batch) -> Result<Vec<u32>, Error> {
        let rows = batch.num_rows();
        match self.columns.len() {
            0 => {
                if rows > 0 && self.group_parts.is_empty() {
                    self.new_group(Box::new([]));
                }
                Ok(vec![0; rows])
            }
            1 => {
                let parts = self.column_parts(batch, 0)?;
                Ok(parts
                    .into_iter()
                    .map(
                        |part| match self.group_of_part.get(part as usize).copied().flatten() {
                            Some(group) => group,
                            None => self.new_group(Box::new([part])),
                        },
                    )
                    .collect())
            }
            width => {
                let columns = (0..width)
                    .map(|index| self.column_parts(batch, index))
                    .collect::<Result<Vec<_>, _>>()?;
                let mut key = vec![0u32; width];
                Ok((0..rows)
                    .map(|row| {
                        for (slot, parts) in key.iter_mut().zip(&columns) {
                            *slot = parts[row];
                        }
                        match self.group_of_parts.get(key.as_slice()) {
                            Some(&group) => group,
                            None => self.new_group(key.clone().into_boxed_slice()),
                        }
                    })
                    .collect())
            }
        }
    }

    /// Group `group`'s value in group column `index`.
    pub(crate) fn label(&self, group: u32, index: usize) -> &Value {
        let part = self.group_parts[group as usize][index];
        &self.parts[index].values[part as usize]
    }

    /// The groups in the order of their group values' keys: the order the
    /// row-at-a-time operators emitted groups in (a `BTreeMap` by key).
    /// Each column's parts are ranked once by key; groups sort by rank.
    pub(crate) fn groups_in_key_order(&self) -> Result<Vec<u32>, Error> {
        let ranks = self
            .parts
            .iter()
            .map(|column| {
                let mut order = (0..column.keys.len()).collect::<Vec<_>>();
                order.sort_unstable_by(|&a, &b| column.keys[a].cmp(&column.keys[b]));
                let mut rank = vec![0u32; order.len()];
                for (position, part) in order.into_iter().enumerate() {
                    rank[part] = position as u32;
                }
                rank
            })
            .collect::<Vec<_>>();
        let mut groups = (0..self.len() as u32).collect::<Vec<_>>();
        groups.sort_unstable_by(|&a, &b| {
            let (a, b) = (&self.group_parts[a as usize], &self.group_parts[b as usize]);
            a.iter()
                .zip(b.iter())
                .zip(&ranks)
                .map(|((&a, &b), rank)| rank[a as usize].cmp(&rank[b as usize]))
                .find(|order| order.is_ne())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(groups)
    }

    /// The group columns of the groups `order`, in the input's types.
    pub(crate) fn label_arrays(
        &self,
        input: &crate::values::SchemaRef,
        order: &[u32],
    ) -> Result<Vec<ArrayRef>, Error> {
        self.columns
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
            let order = table.groups_in_key_order().unwrap();
            for (group, rows) in order.iter().zip(reference.values()) {
                assert!(rows.iter().all(|&row| ids[row] == *group), "{columns:?}");
            }
        }
    }
}
