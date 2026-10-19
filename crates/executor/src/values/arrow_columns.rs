//! The Arrow layout of each Planner column type, and the conversions between
//! a column of [`Value`]s and its Arrow array.
//!
//! Every Planner type has exactly one Arrow layout, so operators never check
//! which of several encodings a column uses:
//!
//! | Planner type | Arrow array |
//! | --- | --- |
//! | `Null` | `NullArray` |
//! | `Bool` | `BooleanArray` |
//! | `Int64` | `Int64Array` |
//! | `Float64` | `Float64Array` |
//! | `Utf8` (a batch column) | `DictionaryArray<UInt32Type>` over a `StringArray` |
//! | `Utf8` (inside a list, struct or map) | `StringArray` |
//! | `Timestamp` | `TimestampMillisecondArray` |
//! | `Date` | `Date32Array` |
//! | `Interval` | `IntervalMonthDayNanoArray` |
//! | `List` / `Struct` / `Map` | `ListArray` / `StructArray` / `MapArray` |
//!
//! A batch column of text is dictionary encoded: one `u32` key per row into
//! the distinct values. For a PromQL source the series identity and label
//! columns therefore carry one key per row and one string per series, and an
//! expression over them is evaluated once per distinct key (see
//! `expressions::columnar`).
//!
//! Arrow fields are always declared nullable here; the Planner schema owns
//! nullability and [`super::Batch::try_new`] enforces it.
use crate::{values::Value, Error};
use ahash::AHashMap as HashMap;
use arrow::array::{
    Array, ArrayRef, BooleanArray, Date32Array, DictionaryArray, Float64Array, Int64Array,
    IntervalMonthDayNanoArray, ListArray, MapArray, NullArray, StringArray, StructArray,
    TimestampMillisecondArray, UInt32Array,
};
use arrow::buffer::{NullBuffer, OffsetBuffer};
use arrow::datatypes::{
    DataType as ArrowType, Field as ArrowField, Fields, IntervalMonthDayNano, TimeUnit, UInt32Type,
};
use planner_types::ir::schema::DataType;
use std::sync::Arc;

/// Where a column sits: a batch column, whose text is dictionary encoded, or
/// a child of a list, struct or map, whose text is a plain string array.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ColumnDepth {
    /// A column of the batch itself.
    BatchColumn,
    /// A child array of a nested column.
    Nested,
}

/// A `Value::Null` that outlives every borrowed row.
static NULL: Value = Value::Null;

/// The Arrow type a Planner column of `dtype` is stored as.
pub(crate) fn arrow_type(dtype: &DataType, depth: ColumnDepth) -> ArrowType {
    match dtype {
        DataType::Null => ArrowType::Null,
        DataType::Bool => ArrowType::Boolean,
        DataType::Int64 => ArrowType::Int64,
        DataType::Float64 => ArrowType::Float64,
        DataType::Utf8 if depth == ColumnDepth::BatchColumn => {
            ArrowType::Dictionary(Box::new(ArrowType::UInt32), Box::new(ArrowType::Utf8))
        }
        DataType::Utf8 => ArrowType::Utf8,
        DataType::Timestamp => ArrowType::Timestamp(TimeUnit::Millisecond, None),
        DataType::Date => ArrowType::Date32,
        DataType::Interval => ArrowType::Interval(arrow::datatypes::IntervalUnit::MonthDayNano),
        DataType::List { element } => ArrowType::List(list_field(&element.name, &element.dtype)),
        DataType::Struct { fields } => ArrowType::Struct(struct_fields(fields)),
        DataType::Map { key, value, .. } => ArrowType::Map(map_entries_field(key, value), false),
    }
}

fn list_field(name: &str, element: &DataType) -> Arc<ArrowField> {
    Arc::new(ArrowField::new(
        name,
        arrow_type(element, ColumnDepth::Nested),
        true,
    ))
}

fn struct_fields(fields: &[planner_types::ir::schema::Field<DataType>]) -> Fields {
    fields
        .iter()
        .map(|field| {
            ArrowField::new(
                &field.name,
                arrow_type(&field.dtype, ColumnDepth::Nested),
                true,
            )
        })
        .collect()
}

fn map_entries_field(key: &DataType, value: &DataType) -> Arc<ArrowField> {
    Arc::new(ArrowField::new(
        "entries",
        ArrowType::Struct(map_entry_fields(key, value)),
        false,
    ))
}

fn map_entry_fields(key: &DataType, value: &DataType) -> Fields {
    Fields::from(vec![
        ArrowField::new("key", arrow_type(key, ColumnDepth::Nested), false),
        ArrowField::new("value", arrow_type(value, ColumnDepth::Nested), true),
    ])
}

fn arrow_error(error: arrow::error::ArrowError) -> Error {
    Error::Invalid(format!("columnar encoding: {error}"))
}

fn nulls_of(values: &[&Value]) -> Option<NullBuffer> {
    if values.iter().any(|v| matches!(v, Value::Null)) {
        Some(NullBuffer::from_iter(
            values.iter().map(|v| !matches!(v, Value::Null)),
        ))
    } else {
        None
    }
}

/// The Arrow array of `values`, a column of Planner type `dtype`. The values
/// must already match the type (checked by [`Value::matches`]); a value of
/// another type is an internal bug and reported as bad input.
pub(crate) fn encode(
    dtype: &DataType,
    depth: ColumnDepth,
    values: &[&Value],
) -> Result<ArrayRef, Error> {
    let mismatch = || Error::Invalid("value differs from its column type".into());
    macro_rules! primitive {
        ($array:ty, $pattern:pat => $value:expr) => {{
            let array: $array = values
                .iter()
                .map(|value| match value {
                    $pattern => Ok(Some($value)),
                    Value::Null => Ok(None),
                    _ => Err(mismatch()),
                })
                .collect::<Result<_, Error>>()?;
            Arc::new(array) as ArrayRef
        }};
    }
    Ok(match dtype {
        DataType::Null => {
            if values.iter().any(|v| !matches!(v, Value::Null)) {
                return Err(mismatch());
            }
            Arc::new(NullArray::new(values.len()))
        }
        DataType::Bool => primitive!(BooleanArray, Value::Bool(v) => *v),
        DataType::Int64 => primitive!(Int64Array, Value::Int64(v) => *v),
        DataType::Float64 => primitive!(Float64Array, Value::Float64(v) => *v),
        DataType::Timestamp => primitive!(TimestampMillisecondArray, Value::Timestamp(v) => *v),
        DataType::Date => primitive!(Date32Array, Value::Date(v) => *v),
        DataType::Interval => primitive!(
            IntervalMonthDayNanoArray,
            Value::Interval { months, days, nanos } => IntervalMonthDayNano {
                months: *months,
                days: *days,
                nanoseconds: *nanos,
            }
        ),
        DataType::Utf8 if depth == ColumnDepth::BatchColumn => {
            let mut codes: HashMap<&str, u32> = HashMap::new();
            let mut dictionary: Vec<&str> = Vec::new();
            let keys: UInt32Array = values
                .iter()
                .map(|value| match value {
                    Value::Utf8(text) => {
                        let next = dictionary.len() as u32;
                        let code = *codes.entry(text.as_ref()).or_insert_with(|| {
                            dictionary.push(text.as_ref());
                            next
                        });
                        Ok(Some(code))
                    }
                    Value::Null => Ok(None),
                    _ => Err(mismatch()),
                })
                .collect::<Result<_, Error>>()?;
            Arc::new(
                DictionaryArray::<UInt32Type>::try_new(
                    keys,
                    Arc::new(StringArray::from(dictionary)),
                )
                .map_err(arrow_error)?,
            )
        }
        DataType::Utf8 => {
            let array: StringArray = values
                .iter()
                .map(|value| match value {
                    Value::Utf8(text) => Ok(Some(text.as_ref())),
                    Value::Null => Ok(None),
                    _ => Err(mismatch()),
                })
                .collect::<Result<_, Error>>()?;
            Arc::new(array)
        }
        DataType::List { element } => {
            let mut children = Vec::new();
            let mut lengths = Vec::with_capacity(values.len());
            for value in values {
                match value {
                    Value::List(items) => {
                        children.extend(items.iter());
                        lengths.push(items.len());
                    }
                    Value::Null => lengths.push(0),
                    _ => return Err(mismatch()),
                }
            }
            let child = encode(&element.dtype, ColumnDepth::Nested, &children)?;
            Arc::new(
                ListArray::try_new(
                    list_field(&element.name, &element.dtype),
                    OffsetBuffer::from_lengths(lengths),
                    child,
                    nulls_of(values),
                )
                .map_err(arrow_error)?,
            )
        }
        DataType::Struct { fields } => {
            let mut children = Vec::with_capacity(fields.len());
            for (index, field) in fields.iter().enumerate() {
                let column = values
                    .iter()
                    .map(|value| match value {
                        Value::Struct(items) => items.get(index).ok_or_else(mismatch),
                        Value::Null => Ok(&NULL),
                        _ => Err(mismatch()),
                    })
                    .collect::<Result<Vec<_>, Error>>()?;
                children.push(encode(&field.dtype, ColumnDepth::Nested, &column)?);
            }
            Arc::new(
                StructArray::try_new_with_length(
                    struct_fields(fields),
                    children,
                    nulls_of(values),
                    values.len(),
                )
                .map_err(arrow_error)?,
            )
        }
        DataType::Map { key, value, .. } => {
            let mut keys = Vec::new();
            let mut items = Vec::new();
            let mut lengths = Vec::with_capacity(values.len());
            for cell in values {
                match cell {
                    Value::Map(entries) => {
                        for (k, v) in entries.iter() {
                            keys.push(k);
                            items.push(v);
                        }
                        lengths.push(entries.len());
                    }
                    Value::Null => lengths.push(0),
                    _ => return Err(mismatch()),
                }
            }
            let entries = StructArray::try_new_with_length(
                map_entry_fields(key, value),
                vec![
                    encode(key, ColumnDepth::Nested, &keys)?,
                    encode(value, ColumnDepth::Nested, &items)?,
                ],
                None,
                keys.len(),
            )
            .map_err(arrow_error)?;
            Arc::new(
                MapArray::try_new(
                    map_entries_field(key, value),
                    OffsetBuffer::from_lengths(lengths),
                    entries,
                    nulls_of(values),
                    false,
                )
                .map_err(arrow_error)?,
            )
        }
    })
}

/// The Arrow array of `length` copies of `value`, a column of `dtype`. Text
/// is one dictionary entry shared by every row.
pub(crate) fn repeat(
    dtype: &DataType,
    depth: ColumnDepth,
    value: &Value,
    length: usize,
) -> Result<ArrayRef, Error> {
    if let (DataType::Utf8, ColumnDepth::BatchColumn, Value::Utf8(text)) = (dtype, depth, value) {
        return Ok(Arc::new(
            DictionaryArray::<UInt32Type>::try_new(
                UInt32Array::from(vec![0u32; length]),
                Arc::new(StringArray::from(vec![text.as_ref()])),
            )
            .map_err(arrow_error)?,
        ));
    }
    encode(dtype, depth, &vec![value; length])
}

/// The texts of a dictionary-encoded column, each decoded once, on first
/// use. A slot per entry when the dictionary is not much larger than the
/// rows read (`rows`), else a map, so reading a slice of a column with a
/// large dictionary costs its rows, not its dictionary.
pub(crate) struct DictionaryTexts<'a> {
    /// The dictionary's distinct texts.
    values: &'a StringArray,
    /// The texts decoded so far.
    decoded: DecodedTexts,
}

/// The texts a [`DictionaryTexts`] decoded so far, by entry.
enum DecodedTexts {
    Dense(Vec<Option<Arc<str>>>),
    Sparse(HashMap<u32, Arc<str>>),
}

/// Whether a per-entry table of a `entries`-entry dictionary is worth it
/// for reading `rows` rows.
pub(crate) fn dense_dictionary(entries: usize, rows: usize) -> bool {
    entries <= 4 * rows + 64
}

impl<'a> DictionaryTexts<'a> {
    /// The texts of `array`, to be read for about `rows` rows.
    pub(crate) fn new(array: &'a DictionaryArray<UInt32Type>, rows: usize) -> Self {
        let values = array
            .values()
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("batch text columns are dictionaries over strings");
        let decoded = if dense_dictionary(values.len(), rows) {
            DecodedTexts::Dense(vec![None; values.len()])
        } else {
            DecodedTexts::Sparse(HashMap::new())
        };
        Self { values, decoded }
    }

    /// The text of dictionary entry `entry`.
    pub(crate) fn get(&mut self, entry: u32) -> Arc<str> {
        let values = self.values;
        let make = || Arc::<str>::from(values.value(entry as usize));
        match &mut self.decoded {
            DecodedTexts::Dense(texts) => texts[entry as usize].get_or_insert_with(make).clone(),
            DecodedTexts::Sparse(texts) => texts.entry(entry).or_insert_with(make).clone(),
        }
    }
}

/// Every value of `array`, a column of Planner type `dtype`.
pub(crate) fn decode(array: &dyn Array, dtype: &DataType, depth: ColumnDepth) -> Vec<Value> {
    let length = array.len();
    let null = |i: usize| array.is_null(i);
    macro_rules! primitive {
        ($array:ty, $value:expr) => {{
            let typed = array
                .as_any()
                .downcast_ref::<$array>()
                .expect("column has its declared layout");
            (0..length)
                .map(|i| {
                    if null(i) {
                        Value::Null
                    } else {
                        #[allow(clippy::redundant_closure_call)]
                        ($value)(typed.value(i))
                    }
                })
                .collect()
        }};
    }
    match dtype {
        DataType::Null => vec![Value::Null; length],
        DataType::Bool => primitive!(BooleanArray, Value::Bool),
        DataType::Int64 => primitive!(Int64Array, Value::Int64),
        DataType::Float64 => primitive!(Float64Array, Value::Float64),
        DataType::Timestamp => primitive!(TimestampMillisecondArray, Value::Timestamp),
        DataType::Date => primitive!(Date32Array, Value::Date),
        DataType::Interval => primitive!(IntervalMonthDayNanoArray, |v: IntervalMonthDayNano| {
            Value::Interval {
                months: v.months,
                days: v.days,
                nanos: v.nanoseconds,
            }
        }),
        DataType::Utf8 if depth == ColumnDepth::BatchColumn => {
            let typed = array
                .as_any()
                .downcast_ref::<DictionaryArray<UInt32Type>>()
                .expect("batch text columns are dictionaries");
            let mut texts = DictionaryTexts::new(typed, length);
            let keys = typed.keys();
            (0..length)
                .map(|i| {
                    if keys.is_null(i) {
                        Value::Null
                    } else {
                        Value::Utf8(texts.get(keys.value(i)))
                    }
                })
                .collect()
        }
        DataType::Utf8 => primitive!(StringArray, |v: &str| Value::Utf8(v.into())),
        DataType::List { element } => {
            let typed = array
                .as_any()
                .downcast_ref::<ListArray>()
                .expect("list column");
            let children = decode(typed.values().as_ref(), &element.dtype, ColumnDepth::Nested);
            let offsets = typed.value_offsets();
            (0..length)
                .map(|i| {
                    if null(i) {
                        Value::Null
                    } else {
                        let (start, end) = (offsets[i] as usize, offsets[i + 1] as usize);
                        Value::List(children[start..end].iter().cloned().collect())
                    }
                })
                .collect()
        }
        DataType::Struct { fields } => {
            let typed = array
                .as_any()
                .downcast_ref::<StructArray>()
                .expect("struct column");
            let children = fields
                .iter()
                .zip(typed.columns())
                .map(|(field, child)| decode(child.as_ref(), &field.dtype, ColumnDepth::Nested))
                .collect::<Vec<_>>();
            (0..length)
                .map(|i| {
                    if null(i) {
                        Value::Null
                    } else {
                        Value::Struct(children.iter().map(|child| child[i].clone()).collect())
                    }
                })
                .collect()
        }
        DataType::Map { key, value, .. } => {
            let typed = array
                .as_any()
                .downcast_ref::<MapArray>()
                .expect("map column");
            let keys = decode(typed.keys().as_ref(), key, ColumnDepth::Nested);
            let items = decode(typed.values().as_ref(), value, ColumnDepth::Nested);
            let offsets = typed.value_offsets();
            (0..length)
                .map(|i| {
                    if null(i) {
                        Value::Null
                    } else {
                        let (start, end) = (offsets[i] as usize, offsets[i + 1] as usize);
                        Value::Map(
                            (start..end)
                                .map(|j| (keys[j].clone(), items[j].clone()))
                                .collect(),
                        )
                    }
                })
                .collect()
        }
    }
}
