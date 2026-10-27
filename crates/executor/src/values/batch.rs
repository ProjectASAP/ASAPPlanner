//! A batch is a set of equal-length Arrow columns in one Planner schema,
//! with summary states held beside them.
use super::arrow_columns::{self, ColumnDepth};
use super::{validate_schema, validate_state, SchemaRef, StateColumn, Value};
use crate::{AggregateCore, Error};
use arrow::array::{Array, ArrayRef, BooleanArray, UInt32Array};
use arrow::datatypes::{DataType as ArrowType, Field as ArrowField, Schema as ArrowSchema};
use arrow::record_batch::{RecordBatch, RecordBatchOptions};
use planner_types::ir::schema::FieldDataType;
use std::sync::Arc;

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
}

impl std::fmt::Debug for Batch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Batch")
            .field("schema", &self.schema)
            .field("num_rows", &self.num_rows())
            .finish()
    }
}

/// One column of a batch built with [`Batch::try_from_columns`].
pub enum BatchColumn {
    /// A plain column: an Arrow array in its Planner type's layout (see
    /// [`super::arrow_columns`]: text is `DictionaryArray<UInt32Type>` over
    /// `Utf8`, a timestamp `TimestampMillisecondArray`, …).
    Plain(ArrayRef),
    /// A summary column: row `i`'s state is `states.state(indices[i])`.
    States {
        /// Each row's state index; never NULL.
        indices: UInt32Array,
        /// The states, e.g. a sealed [`crate::StateColumnUpdater`].
        states: Arc<StateColumn>,
    },
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
    /// The batch of `rows` in `schema`, encoded into columns (the rows are
    /// not kept). For literal rows (`VALUES`, constants, tests); runtimes
    /// build batches from columns with [`Self::try_from_columns`]. Bad input
    /// when a row's width or a value's type differs from the schema, or a
    /// summary state differs from its declared family or parameters.
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
        Self::from_parts(schema, arrays, state_columns, rows.len())
    }

    /// The batch of `columns` in `schema`, one per field, without running
    /// the DAG: how a runtime installs states it built sample by sample
    /// (a sealed [`crate::StateColumnUpdater`]) as a stored pane. Bad input
    /// when a column's count, layout, length or nullability differs from
    /// the schema, a dictionary holds a NULL value (a NULL is a NULL key),
    /// a summary field's states are of another family, or a state index is
    /// NULL or past its states.
    ///
    /// A stored pane has one row per group: the caller must not repeat a
    /// group's labels (two rows of one series would be merged as two
    /// panes of it). This is not checked, as it would cost a pass grouping
    /// every row.
    pub fn try_from_columns(schema: SchemaRef, columns: Vec<BatchColumn>) -> Result<Self, Error> {
        validate_schema(&schema)?;
        if columns.len() != schema.fields.len() {
            return Err(Error::Invalid(
                "column count differs from Planner schema".into(),
            ));
        }
        let row_count = columns.first().map_or(0, |column| match column {
            BatchColumn::Plain(array) => array.len(),
            BatchColumn::States { indices, .. } => indices.len(),
        });
        let mut arrays = Vec::with_capacity(columns.len());
        let mut state_columns = Vec::with_capacity(columns.len());
        for (column, field) in columns.into_iter().zip(&schema.fields) {
            match (column, &field.dtype) {
                (BatchColumn::Plain(array), FieldDataType::Plain(dtype)) => {
                    // A NULL is a NULL key, never a NULL dictionary value,
                    // so every dictionary read finds a text.
                    if array
                        .as_any()
                        .downcast_ref::<arrow::array::DictionaryArray<arrow::datatypes::UInt32Type>>()
                        .is_some_and(|dictionary| dictionary.values().null_count() > 0)
                    {
                        return Err(Error::Invalid(format!(
                            "column {} has a NULL dictionary value; a NULL is a NULL key",
                            field.name
                        )));
                    }
                    if array.data_type()
                        != &arrow_columns::arrow_type(dtype, ColumnDepth::BatchColumn)
                    {
                        return Err(Error::Invalid(format!(
                            "column {} is not in its type's Arrow layout",
                            field.name
                        )));
                    }
                    arrays.push(array);
                    state_columns.push(None);
                }
                (BatchColumn::States { indices, states }, family)
                    if !matches!(family, FieldDataType::Plain(_)) =>
                {
                    if states.family() != family {
                        return Err(Error::Invalid(format!(
                            "states of {} are of another family",
                            field.name
                        )));
                    }
                    if indices.null_count() > 0
                        || indices.values().iter().any(|&i| i as usize >= states.len())
                    {
                        return Err(Error::Invalid(format!(
                            "a state index of {} is NULL or past its states",
                            field.name
                        )));
                    }
                    arrays.push(Arc::new(indices));
                    state_columns.push(Some(states));
                }
                _ => {
                    return Err(Error::Invalid(format!(
                        "column {} is a summary column exactly when its field is",
                        field.name
                    )))
                }
            }
        }
        if arrays.iter().any(|array| array.len() != row_count) {
            return Err(Error::Invalid("columns differ in length".into()));
        }
        Self::from_parts(schema, arrays, state_columns, row_count)
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
            if !field.nullable
                && field.dtype != FieldDataType::Plain(planner_types::ir::schema::DataType::Null)
                && array.logical_null_count() > 0
            {
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
        Self {
            schema: self.schema.clone(),
            record_batch,
            state_columns: self.state_columns.clone(),
        }
        .compacted()
    }

    /// This batch with each state column that its rows use less than half
    /// of reduced to the states they use (exact records kept as records),
    /// so a selective filter does not hold every state of its input.
    fn compacted(self) -> Result<Self, Error> {
        let mut arrays = self.record_batch.columns().to_vec();
        let mut states = self.state_columns.to_vec();
        let mut changed = false;
        for (column, slot) in states.iter_mut().enumerate() {
            let Some(state_column) = slot.clone() else {
                continue;
            };
            let indices = arrays[column]
                .as_any()
                .downcast_ref::<UInt32Array>()
                .expect("summary columns hold state indices")
                .clone();
            if 2 * indices.len() >= state_column.len() {
                continue;
            }
            let mut moved = ahash::AHashMap::<u32, u32>::new();
            let mut used = Vec::new();
            let remapped = indices
                .values()
                .iter()
                .map(|&index| {
                    *moved.entry(index).or_insert_with(|| {
                        used.push(index as usize);
                        (used.len() - 1) as u32
                    })
                })
                .collect::<UInt32Array>();
            if used.len() == state_column.len() {
                continue;
            }
            let family = state_column.family().clone();
            *slot = Some(Arc::new(match state_column.storage() {
                super::StateStorage::Exact(records) => StateColumn::from_exact(
                    family,
                    used.iter().map(|&index| records[index].clone()).collect(),
                ),
                // Shared by pointer: stored sketches are not charged again.
                super::StateStorage::Objects(_) => StateColumn::from_objects_of(
                    family,
                    used.iter()
                        .map(|&index| {
                            (
                                state_column.state(index),
                                state_column.state_held_outside_run(index),
                            )
                        })
                        .collect(),
                ),
            }));
            arrays[column] = Arc::new(remapped);
            changed = true;
        }
        if !changed {
            return Ok(self);
        }
        let rows = self.num_rows();
        Self::from_parts(self.schema, arrays, states, rows)
    }

    /// The rows `indices` (in that order, repeats allowed), sharing the
    /// state columns. A NULL index gives a row of NULLs, which a summary
    /// column cannot hold (bad input).
    pub(crate) fn take(&self, indices: &UInt32Array) -> Result<Self, Error> {
        if indices.null_count() > 0 && self.state_columns.iter().any(Option::is_some) {
            return Err(Error::Invalid("a summary column has no NULL row".into()));
        }
        let arrays = self
            .record_batch
            .columns()
            .iter()
            .map(|array| {
                arrow::compute::take(array.as_ref(), indices, None)
                    .map_err(|error| Error::Invalid(format!("columnar take: {error}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Self::from_parts(
            self.schema.clone(),
            arrays,
            self.state_columns.to_vec(),
            indices.len(),
        )
    }

    /// The columns `columns` of this batch (shared, not copied), as a batch
    /// of `schema`.
    pub(crate) fn project(&self, schema: SchemaRef, columns: &[usize]) -> Result<Self, Error> {
        Self::from_parts(
            schema,
            columns.iter().map(|&c| self.column(c).clone()).collect(),
            columns
                .iter()
                .map(|&c| self.state_column(c).cloned())
                .collect(),
            self.num_rows(),
        )
    }

    /// This batch with column `index` replaced by `array` (a plain column
    /// of its field's layout).
    pub(crate) fn with_plain_column(&self, index: usize, array: ArrayRef) -> Result<Self, Error> {
        let mut arrays = self.record_batch.columns().to_vec();
        let mut states = self.state_columns.to_vec();
        arrays[index] = array;
        states[index] = None;
        Self::from_parts(self.schema.clone(), arrays, states, self.num_rows())
    }

    /// `batches`, all in `schema`, as one batch: plain columns copied,
    /// state columns joined (shared when every batch shares one).
    pub(crate) fn concat(schema: &SchemaRef, batches: &[Self]) -> Result<Self, Error> {
        match batches {
            [] => return Self::try_new(schema.clone(), Vec::new()),
            [batch] => return Ok(batch.clone()),
            _ => {}
        }
        let rows = batches.iter().map(Self::num_rows).sum();
        let mut arrays = Vec::with_capacity(schema.fields.len());
        let mut states = Vec::with_capacity(schema.fields.len());
        for column in 0..schema.fields.len() {
            let pieces = batches
                .iter()
                .map(|batch| batch.column(column).as_ref())
                .collect::<Vec<_>>();
            let columns = batches
                .iter()
                .map(|batch| batch.state_column(column))
                .collect::<Vec<_>>();
            let Some(Some(first)) = columns.first().copied() else {
                arrays.push(
                    arrow::compute::concat(&pieces)
                        .map_err(|error| Error::Invalid(format!("columnar concat: {error}")))?,
                );
                states.push(None);
                continue;
            };
            if columns
                .iter()
                .all(|other| other.is_some_and(|other| Arc::ptr_eq(other, first)))
            {
                arrays.push(
                    arrow::compute::concat(&pieces)
                        .map_err(|error| Error::Invalid(format!("columnar concat: {error}")))?,
                );
                states.push(Some(first.clone()));
                continue;
            }
            // Different state columns: one column of the states the rows
            // reference (each once), exact records kept as records.
            let all_exact = columns.iter().all(|column| {
                matches!(
                    column.map(|column| column.storage()),
                    Some(super::StateStorage::Exact(_))
                )
            });
            let mut indices = Vec::with_capacity(rows);
            let mut objects = Vec::new();
            let mut records = Vec::new();
            for (batch, state_column) in batches.iter().zip(&columns) {
                let state_column = state_column.expect("one schema");
                let local = batch
                    .column(column)
                    .as_any()
                    .downcast_ref::<UInt32Array>()
                    .expect("summary columns hold state indices");
                let mut moved = ahash::AHashMap::<u32, u32>::new();
                for &index in local.values().iter() {
                    let next = (objects.len() + records.len()) as u32;
                    let new_index = *moved.entry(index).or_insert_with(|| {
                        match state_column.storage() {
                            super::StateStorage::Exact(states) if all_exact => {
                                records.push(states[index as usize].clone())
                            }
                            // An object is shared by pointer (not charged
                            // again when held outside the run); an exact
                            // record becomes a new object.
                            super::StateStorage::Objects(_) => objects.push((
                                state_column.state(index as usize),
                                state_column.state_held_outside_run(index as usize),
                            )),
                            super::StateStorage::Exact(_) => {
                                objects.push((state_column.state(index as usize), false))
                            }
                        }
                        next
                    });
                    indices.push(new_index);
                }
            }
            arrays.push(Arc::new(UInt32Array::from(indices)) as ArrayRef);
            let family = first.family().clone();
            states.push(Some(Arc::new(if all_exact {
                StateColumn::from_exact(family, records)
            } else {
                StateColumn::from_objects_of(family, objects)
            })));
        }
        Self::from_parts(schema.clone(), arrays, states, rows)
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

    /// The rows as values, decoded from the columns on every call: for
    /// results leaving the executor and for tests. Operators read columns.
    pub fn to_rows(&self) -> Vec<Vec<Value>> {
        {
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
        }
    }

    /// Records that something outside any run (the caller of a plan, a
    /// store of panes) holds this batch's states, so runs that read them
    /// are not charged for them again. States a run copies or creates from
    /// them are new state columns and are charged.
    pub(crate) fn mark_states_held_outside_run(&self) {
        for column in self.state_columns.iter().flatten() {
            column.mark_held_outside_run();
        }
    }

    /// Memory the batch holds, for execution memory reservations: its
    /// column buffers, a fixed slot per row, and each state column once,
    /// except states held outside the run (see
    /// [`Self::mark_states_held_outside_run`]). Constant time in the number
    /// of rows.
    pub fn memory_bytes(&self) -> usize {
        let mut seen = std::collections::BTreeSet::new();
        let states = self
            .state_columns
            .iter()
            .flatten()
            .filter(|column| seen.insert(Arc::as_ptr(column)))
            .map(|column| column.charged_state_bytes())
            .sum::<usize>();
        std::mem::size_of::<Self>()
            + self.record_batch.get_array_memory_size()
            + self.num_rows() * ROW_SLOT_BYTES
            + states
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
        assert_eq!(
            many.memory_bytes() - none.memory_bytes(),
            200 * ROW_SLOT_BYTES
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

    // A runtime turns sealed per-sample states and their group columns into
    // a batch: the states and labels read back as the rows say.
    #[test]
    fn a_batch_from_columns_holds_sealed_states_and_labels() {
        use crate::StateColumnUpdater;
        use planner_types::ir::schema::{ExactKind, ExactParams};
        let family = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
        let mut updater = StateColumnUpdater::new(&family).unwrap();
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
                    table: None,
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
                .evaluation(planner_types::ir::schema::ExactKind::Sum, None, None)
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
            StateColumnUpdater::new(&FieldDataType::ExactAggregate(
                ExactKind::Count,
                ExactParams::Count,
            ))
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
        let batch = Batch::try_from_columns(nullable, vec![BatchColumn::Plain(Arc::new(null_key))])
            .unwrap();
        let rows = batch.to_rows();
        assert!(matches!(&rows[0][0], Value::Utf8(job) if job.as_ref() == "a"));
        assert!(matches!(rows[1][0], Value::Null));
        let nulls = batch.column(0).logical_nulls().unwrap();
        assert!(nulls.is_valid(0) && nulls.is_null(1));
    }

    // A filter that keeps few rows keeps only their states: a pane filtered
    // to two of ten series holds two states, not ten.
    #[test]
    fn a_selective_filter_compacts_its_state_column() {
        use crate::StateColumnUpdater;
        use planner_types::ir::schema::{ExactKind, ExactParams};
        let family = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
        let mut updater = StateColumnUpdater::new(&family).unwrap();
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
                    table: None,
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
                    .evaluation(planner_types::ir::schema::ExactKind::Sum, None, None)
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
        use planner_types::ir::schema::{ExactKind, ExactParams};
        let family = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
        let schema = Arc::new(Schema {
            fields: vec![Field {
                name: "state".into(),
                dtype: family.clone(),
                nullable: false,
                table: None,
            }],
            time_index: None,
            unique_keys: vec![],
            closed: false,
        });
        // States [10, 20, 30] referenced as rows [2, 0, 2]; states [1, 2]
        // referenced as rows [1].
        let batch = |sums: &[f64], rows: Vec<u32>| {
            let mut updater = StateColumnUpdater::new(&family).unwrap();
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
                    .evaluation(planner_types::ir::schema::ExactKind::Sum, None, None)
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
        use planner_types::ir::schema::{
            ExactKind, ExactParams, SketchAlgorithm, SketchKind, SketchParams,
        };
        let family = if exact {
            FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum)
        } else {
            FieldDataType::Sketch(
                SketchKind::new(
                    SketchAlgorithm::Kll,
                    SketchParams::Kll {
                        k: 200,
                        failure_probability: None,
                    },
                ),
                Default::default(),
            )
        };
        let mut updater = StateColumnUpdater::new(&family).unwrap();
        updater.ensure_groups(count as usize).unwrap();
        for group in 0..count {
            updater.update(group, f64::from(group), 0).unwrap();
        }
        let schema = Arc::new(Schema {
            fields: vec![Field {
                name: "state".into(),
                dtype: family,
                nullable: false,
                table: None,
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
}
