//! A batch is a set of equal-length Arrow columns in one Planner schema,
//! with summary states held beside them.
use super::arrow_columns::{self, ColumnDepth};
use super::{validate_schema, validate_state, SchemaRef, StateColumn, Value};
use crate::{AggregateCore, Error};
use arrow::array::{Array, ArrayRef, BooleanArray, UInt32Array};
use arrow::datatypes::{DataType as ArrowType, Field as ArrowField, Schema as ArrowSchema};
use arrow::record_batch::{RecordBatch, RecordBatchOptions};
use planner_types::ir::schema::FieldDataType;
use std::sync::{Arc, OnceLock};

/// Each row is charged this many bytes on top of its column buffers, so a
/// batch of zero-width rows still consumes memory budget.
const ROW_SLOT_BYTES: usize = 8;

/// The output of one operator: equal-length columns in one Planner schema.
///
/// Plain columns are Arrow arrays in a [`RecordBatch`], in the layout
/// [`arrow_columns`] gives each Planner type. A summary column is a
/// `UInt32` array of state indices in the record batch, plus its
/// [`StateColumn`] beside it. Cloning a batch shares its buffers.
#[derive(Clone)]
pub struct Batch {
    /// The Planner schema, which the columns follow field by field.
    schema: SchemaRef,
    /// One Arrow column per Planner field; a summary field holds state
    /// indices into its entry of `state_columns`.
    record_batch: RecordBatch,
    /// Per Planner field, the states of a summary field; `None` for a plain
    /// field.
    state_columns: Arc<[Option<Arc<StateColumn>>]>,
    /// The rows, decoded from the columns the first time [`Self::rows`] is
    /// called and shared by clones.
    decoded_rows: Arc<OnceLock<Vec<Vec<Value>>>>,
}

impl std::fmt::Debug for Batch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Batch")
            .field("schema", &self.schema)
            .field("num_rows", &self.num_rows())
            .finish()
    }
}

/// The Arrow schema of a Planner schema's columns.
fn arrow_schema(schema: &SchemaRef) -> Arc<ArrowSchema> {
    Arc::new(ArrowSchema::new(
        schema
            .fields
            .iter()
            .map(|field| {
                ArrowField::new(
                    &field.name,
                    match &field.dtype {
                        FieldDataType::Plain(dtype) => {
                            arrow_columns::arrow_type(dtype, ColumnDepth::BatchColumn)
                        }
                        _ => ArrowType::UInt32,
                    },
                    true,
                )
            })
            .collect::<Vec<_>>(),
    ))
}

impl Batch {
    /// The batch of `rows` in `schema`. Bad input when a row's width or a
    /// value's type differs from the schema, or a summary state differs from
    /// its declared family or parameters.
    pub fn try_new(schema: SchemaRef, rows: Vec<Vec<Value>>) -> Result<Self, Error> {
        validate_schema(&schema)?;
        for row in &rows {
            if row.len() != schema.fields.len() {
                return Err(Error::Invalid(
                    "row width differs from Planner schema".into(),
                ));
            }
            for (value, field) in row.iter().zip(&schema.fields) {
                let matches = match (&field.dtype, value) {
                    (FieldDataType::Plain(dtype), value) => value.matches(dtype, field.nullable),
                    (expected, Value::Summary { family, state }) => {
                        expected == family && validate_state(family, state.as_ref()).is_ok()
                    }
                    _ => false,
                };
                if !matches {
                    return Err(Error::Invalid(format!(
                        "value differs from type of {}",
                        field.name
                    )));
                }
            }
        }
        let mut arrays = Vec::with_capacity(schema.fields.len());
        let mut state_columns = Vec::with_capacity(schema.fields.len());
        for (index, field) in schema.fields.iter().enumerate() {
            let cells = rows.iter().map(|row| &row[index]).collect::<Vec<_>>();
            match &field.dtype {
                FieldDataType::Plain(dtype) => {
                    arrays.push(arrow_columns::encode(
                        dtype,
                        ColumnDepth::BatchColumn,
                        &cells,
                    )?);
                    state_columns.push(None);
                }
                family => {
                    // Rows sharing one state object keep sharing it.
                    let mut positions = std::collections::HashMap::new();
                    let mut states: Vec<Arc<dyn AggregateCore>> = Vec::new();
                    let indices = cells
                        .iter()
                        .map(|cell| {
                            let Value::Summary { state, .. } = cell else {
                                unreachable!("validated above");
                            };
                            let identity = Arc::as_ptr(state) as *const () as usize;
                            *positions.entry(identity).or_insert_with(|| {
                                states.push(state.clone());
                                (states.len() - 1) as u32
                            })
                        })
                        .collect::<UInt32Array>();
                    arrays.push(Arc::new(indices));
                    state_columns.push(Some(Arc::new(StateColumn::from_objects(
                        family.clone(),
                        states,
                    ))));
                }
            }
        }
        let batch = Self::from_parts(schema, arrays, state_columns, rows.len())?;
        // The caller's rows are the decoded rows; keep them (charged in
        // `memory_bytes`).
        let _ = batch.decoded_rows.set(rows);
        Ok(batch)
    }

    /// The batch of `arrays` in `schema`, with `state_columns` for its
    /// summary fields. The arrays must have the layout of their Planner
    /// types ([`arrow_columns`]) and `row_count` rows; a summary field's
    /// array holds indices into its state column. Internal callers only.
    pub(crate) fn from_parts(
        schema: SchemaRef,
        arrays: Vec<ArrayRef>,
        state_columns: Vec<Option<Arc<StateColumn>>>,
        row_count: usize,
    ) -> Result<Self, Error> {
        if arrays.len() != schema.fields.len() || state_columns.len() != schema.fields.len() {
            return Err(Error::Invalid(
                "column count differs from Planner schema".into(),
            ));
        }
        for ((array, states), field) in arrays.iter().zip(&state_columns).zip(&schema.fields) {
            let plain = matches!(field.dtype, FieldDataType::Plain(_));
            if plain == states.is_some() {
                return Err(Error::Invalid(format!(
                    "column {} has states exactly when it is a summary column",
                    field.name
                )));
            }
            if !field.nullable && array.null_count() > 0 {
                return Err(Error::Invalid(format!(
                    "value differs from type of {}",
                    field.name
                )));
            }
        }
        let record_batch = RecordBatch::try_new_with_options(
            arrow_schema(&schema),
            arrays,
            &RecordBatchOptions::new().with_row_count(Some(row_count)),
        )
        .map_err(|error| Error::Invalid(format!("columnar batch: {error}")))?;
        Ok(Self {
            schema,
            record_batch,
            state_columns: state_columns.into(),
            decoded_rows: Arc::new(OnceLock::new()),
        })
    }

    /// Rows `offset..offset + length`, sharing the column buffers.
    pub(crate) fn slice(&self, offset: usize, length: usize) -> Self {
        if offset == 0 && length == self.num_rows() {
            return self.clone();
        }
        Self {
            schema: self.schema.clone(),
            record_batch: self.record_batch.slice(offset, length),
            state_columns: self.state_columns.clone(),
            decoded_rows: Arc::new(OnceLock::new()),
        }
    }

    /// The rows where `keep` is true (not false or null), in order. Shares
    /// the state columns; copies only the kept cells of plain columns.
    pub(crate) fn filter(&self, keep: &BooleanArray) -> Result<Self, Error> {
        if keep.true_count() == self.num_rows() && keep.null_count() == 0 {
            return Ok(self.clone());
        }
        let record_batch = arrow::compute::filter_record_batch(&self.record_batch, keep)
            .map_err(|error| Error::Invalid(format!("columnar filter: {error}")))?;
        Ok(Self {
            schema: self.schema.clone(),
            record_batch,
            state_columns: self.state_columns.clone(),
            decoded_rows: Arc::new(OnceLock::new()),
        })
    }

    /// The Planner schema of the columns.
    pub fn schema(&self) -> &SchemaRef {
        &self.schema
    }

    /// The number of rows.
    pub fn num_rows(&self) -> usize {
        self.record_batch.num_rows()
    }

    /// The Arrow columns, one per Planner field. A summary field's column
    /// holds `UInt32` indices into [`Self::state_column`].
    pub fn record_batch(&self) -> &RecordBatch {
        &self.record_batch
    }

    /// The Arrow column of field `index`.
    pub fn column(&self, index: usize) -> &ArrayRef {
        self.record_batch.column(index)
    }

    /// The states of summary field `index`; `None` for a plain field.
    pub fn state_column(&self, index: usize) -> Option<&Arc<StateColumn>> {
        self.state_columns.get(index).and_then(Option::as_ref)
    }

    /// The rows as values, for results leaving the executor and for tests.
    /// Decoded from the columns on the first call, then kept with the batch.
    pub fn rows(&self) -> &[Vec<Value>] {
        self.decoded_rows.get_or_init(|| {
            let length = self.num_rows();
            let width = self.schema.fields.len();
            let mut rows: Vec<Vec<Value>> =
                (0..length).map(|_| Vec::with_capacity(width)).collect();
            for (index, field) in self.schema.fields.iter().enumerate() {
                let array = self.record_batch.column(index);
                let column = match (&field.dtype, self.state_column(index)) {
                    (FieldDataType::Plain(dtype), _) => {
                        arrow_columns::decode(array.as_ref(), dtype, ColumnDepth::BatchColumn)
                    }
                    (_, Some(states)) => {
                        let indices = array
                            .as_any()
                            .downcast_ref::<UInt32Array>()
                            .expect("summary columns hold state indices");
                        indices
                            .values()
                            .iter()
                            .map(|&i| Value::Summary {
                                family: states.family().clone(),
                                state: states.state(i as usize),
                            })
                            .collect()
                    }
                    (_, None) => unreachable!("summary columns have states"),
                };
                for (row, value) in rows.iter_mut().zip(column) {
                    row.push(value);
                }
            }
            rows
        })
    }

    /// Memory the batch holds, for execution memory reservations: its
    /// column buffers, a fixed slot per row, each state column once, and,
    /// when the batch holds its rows as values (kept from [`Self::try_new`]
    /// or decoded by [`Self::rows`]), an estimate of those rows. Constant
    /// time in the number of rows.
    pub fn memory_bytes(&self) -> usize {
        let mut seen = std::collections::BTreeSet::new();
        let states = self
            .state_columns
            .iter()
            .flatten()
            .filter(|column| seen.insert(Arc::as_ptr(column)))
            .map(|column| column.state_memory_bytes())
            .sum::<usize>();
        let rows = self.decoded_rows.get().map_or(0, |rows| {
            rows.capacity() * std::mem::size_of::<Vec<Value>>()
                + rows.len() * self.schema.fields.len() * std::mem::size_of::<Value>()
        });
        std::mem::size_of::<Self>()
            + self.record_batch.get_array_memory_size()
            + self.num_rows() * ROW_SLOT_BYTES
            + states
            + rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::summary_kernels::DDSketchAccumulator;
    use planner_types::ir::schema::{
        DataType, Field, Schema, SketchAlgorithm, SketchKind, SketchParams,
    };

    fn field(name: &str, dtype: DataType, nullable: bool) -> Field {
        Field {
            name: name.into(),
            dtype: FieldDataType::Plain(dtype),
            nullable,
            table: None,
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

    /// The rows decoded from the columns alone, not the rows kept from
    /// construction.
    fn decoded(batch: &Batch) -> Vec<Vec<Value>> {
        Batch::from_parts(
            batch.schema().clone(),
            batch.record_batch().columns().to_vec(),
            batch.state_columns.to_vec(),
            batch.num_rows(),
        )
        .unwrap()
        .rows()
        .to_vec()
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
                table: None,
            },
            Field::<DataType> {
                name: "b".into(),
                dtype: DataType::Utf8,
                nullable: true,
                table: None,
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
        let column: &DictionaryArray<arrow::datatypes::UInt32Type> =
            batch.column(0).as_dictionary();
        assert_eq!(column.values().len(), 2);
        assert_eq!(column.keys().values().to_vec(), vec![0, 1, 0, 0, 1]);
    }

    fn dd_family() -> FieldDataType {
        FieldDataType::Sketch(
            SketchKind::new(
                SketchAlgorithm::DDSketch,
                SketchParams::DDSketch { alpha: 0.01 },
            ),
            Default::default(),
        )
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
                    table: None,
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
        // Both keep their (zero-width) rows: 200 row headers more.
        assert_eq!(
            many.memory_bytes() - none.memory_bytes(),
            200 * (ROW_SLOT_BYTES + std::mem::size_of::<Vec<Value>>())
        );
    }

    // A runtime can share batches, stored panes included, across threads.
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
}
