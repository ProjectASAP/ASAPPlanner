//! A batch is a set of equal-length Arrow columns in one Planner schema,
//! with summary states held beside them.
use super::arrow_columns::{self, ColumnDepth};
use super::{validate_schema, validate_state, SchemaRef, StateColumn, Value};
use crate::values::arrow_columns::text_dictionary_array;
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
/// `arrow_columns` gives each Planner type. A summary column is a
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
    /// `super::arrow_columns`: text is `DictionaryArray<UInt32Type>` over
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
    /// (a sealed [`crate::StateColumnUpdater`]) as a stored tumbling window. Bad input
    /// when a column's count, layout, length or nullability differs from
    /// the schema, a dictionary holds a NULL value (a NULL is a NULL key),
    /// a summary field's states are of another family, or a state index is
    /// NULL or past its states.
    ///
    /// A stored tumbling window has one row per group: the caller must not repeat a
    /// group's labels (two rows of one series would be merged as two
    /// tumbling windows of it). This is not checked, as it would cost a pass grouping
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
                    if text_dictionary_array(&*array)
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

    /// This batch with `column` (a plain column) after its own, as
    /// `schema`, whose last field types it. Shares the existing columns.
    pub(crate) fn with_appended_column(
        &self,
        schema: SchemaRef,
        column: ArrayRef,
    ) -> Result<Self, Error> {
        let mut arrays = self.record_batch.columns().to_vec();
        let mut state_columns = self.state_columns.to_vec();
        arrays.push(column);
        state_columns.push(None);
        Self::from_parts(schema, arrays, state_columns, self.num_rows())
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
            // States in their view layout that something outside the run
            // holds cost the run nothing: indices into them are kept as
            // they are.
            if matches!(
                state_column.storage(),
                super::StateStorage::Mapped(_)
                    | super::StateStorage::Exact(super::state_column::ExactRecords::Mapped(_))
            ) && state_column.is_held_outside_run()
            {
                continue;
            }
            let mut used = Vec::new();
            let remapped = UInt32Array::from(remap_each_distinct(indices.values(), |index| {
                used.push(index as usize);
                (used.len() - 1) as u32
            }));
            if used.len() == state_column.len() {
                continue;
            }
            let family = state_column.family().clone();
            *slot = Some(Arc::new(match state_column.storage() {
                super::StateStorage::Exact(records) => StateColumn::from_exact(
                    family,
                    used.iter().map(|&index| records.get(index)).collect(),
                ),
                // Sealed views the run holds: the used states copied into a
                // buffer of their own, so this batch keeps none of the rest
                // of the source buffer.
                super::StateStorage::Mapped(mapped) => {
                    let (copied, charged) = super::mapped_states::MappedSketches::copied(
                        mapped.kernel(),
                        used.iter().map(|&index| (mapped, index)),
                    )?;
                    StateColumn::stored_views(family, super::StateStorage::Mapped(copied), charged)
                }
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
            // Mapped sketch columns stay mapped. Columns a deployment holds
            // are pointed at, not copied, and charged nothing; when any is
            // sealed views the run holds, the referenced states are copied
            // into one buffer of their own, charged its bytes.
            if let Some(first_mapped) = columns.iter().try_fold(None, |kernel, column| match column
                .map(|column| column.storage())
            {
                Some(super::StateStorage::Mapped(mapped))
                    if kernel.is_none_or(|k| k == mapped.kernel()) =>
                {
                    Some(Some(mapped.kernel()))
                }
                _ => None,
            }) {
                let kernel = first_mapped.expect("at least two columns");
                let mut indices = Vec::with_capacity(rows);
                let mut selected: Vec<(&super::mapped_states::MappedSketches, usize)> = Vec::new();
                let mut any_run_held = false;
                for (batch, state_column) in batches.iter().zip(&columns) {
                    let state_column = state_column.expect("one schema");
                    let super::StateStorage::Mapped(mapped) = state_column.storage() else {
                        unreachable!("checked mapped")
                    };
                    any_run_held |= !state_column.is_held_outside_run();
                    let local = batch
                        .column(column)
                        .as_any()
                        .downcast_ref::<UInt32Array>()
                        .expect("summary columns hold state indices");
                    indices.extend(remap_each_distinct(local.values(), |index| {
                        selected.push((mapped, index as usize));
                        (selected.len() - 1) as u32
                    }));
                }
                arrays.push(Arc::new(UInt32Array::from(indices)) as ArrayRef);
                let family = first.family().clone();
                states.push(Some(Arc::new(if any_run_held {
                    let (copied, charged) =
                        super::mapped_states::MappedSketches::copied(kernel, selected.into_iter())?;
                    StateColumn::stored_views(family, super::StateStorage::Mapped(copied), charged)
                } else {
                    StateColumn::mapped_column(
                        family,
                        super::StateStorage::Mapped(super::mapped_states::MappedSketches::gather(
                            kernel,
                            selected.into_iter(),
                        )),
                        0,
                    )
                })));
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
                indices.extend(remap_each_distinct(local.values(), |index| {
                    let next = (objects.len() + records.len()) as u32;
                    match state_column.storage() {
                        super::StateStorage::Exact(states) if all_exact => {
                            records.push(states.get(index as usize))
                        }
                        // An object is shared by pointer (not charged
                        // again when held outside the run); an exact
                        // record becomes a new object.
                        super::StateStorage::Objects(_) => objects.push((
                            state_column.state(index as usize),
                            state_column.state_held_outside_run(index as usize),
                        )),
                        super::StateStorage::Exact(_) | super::StateStorage::Mapped(_) => {
                            objects.push((state_column.state(index as usize), false))
                        }
                    }
                    next
                }));
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

    /// The rows `indices` of `batches` (all in `schema`), each a (batch,
    /// row) pair, in that order: one copy of each row, where [`Self::concat`]
    /// then [`Self::take`] copy it twice. `[(1, 0), (0, 2)]` is the first row
    /// of `batches[1]`, then the third of `batches[0]`. A state column the
    /// batches do not share is joined as `concat` joins it.
    pub(crate) fn interleave(
        schema: &SchemaRef,
        batches: &[Self],
        indices: &[(usize, usize)],
    ) -> Result<Self, Error> {
        let Some(first) = batches.first() else {
            return Self::try_new(schema.clone(), Vec::new());
        };
        let shared_states = (0..schema.fields.len()).all(|column| {
            batches.iter().all(|batch| {
                match (batch.state_column(column), first.state_column(column)) {
                    (None, None) => true,
                    (Some(states), Some(first)) => Arc::ptr_eq(states, first),
                    _ => false,
                }
            })
        });
        if !shared_states {
            let mut offsets = Vec::with_capacity(batches.len());
            let mut rows = 0u32;
            for batch in batches {
                offsets.push(rows);
                rows += batch.num_rows() as u32;
            }
            return Self::concat(schema, batches)?.take(&UInt32Array::from_iter_values(
                indices
                    .iter()
                    .map(|&(batch, row)| offsets[batch] + row as u32),
            ));
        }
        let arrays = (0..schema.fields.len())
            .map(|column| {
                let pieces = batches
                    .iter()
                    .map(|batch| batch.column(column).as_ref())
                    .collect::<Vec<_>>();
                arrow::compute::interleave(&pieces, indices)
                    .map_err(|error| Error::Invalid(format!("columnar interleave: {error}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Self::from_parts(
            schema.clone(),
            arrays,
            first.state_columns.to_vec(),
            indices.len(),
        )
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
    /// store of tumbling windows) holds this batch's states, so runs that read them
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
    /// `Self::mark_states_held_outside_run`). Constant time in the number
    /// of rows.
    pub fn memory_bytes(&self) -> usize {
        self.bytes(StateColumn::charged_state_bytes)
    }

    /// Memory the batch holds, counting every state column in full: what a
    /// store of tumbling windows reports for the batches it keeps. Unlike
    /// [`Self::memory_bytes`], it does not drop the states once a run has
    /// read them (a run marks a store's states as held outside it, and
    /// the mark is on the store's own state columns). Constant time in the
    /// number of rows.
    pub fn stored_bytes(&self) -> usize {
        self.bytes(StateColumn::state_memory_bytes)
    }

    /// The column buffers, a fixed slot per row, and `state_bytes` of each
    /// state column once.
    fn bytes(&self, state_bytes: fn(&StateColumn) -> usize) -> usize {
        let mut seen = std::collections::BTreeSet::new();
        let states = self
            .state_columns
            .iter()
            .flatten()
            .filter(|column| seen.insert(Arc::as_ptr(column)))
            .map(|column| state_bytes(column))
            .sum::<usize>();
        std::mem::size_of::<Self>()
            + self.record_batch.get_array_memory_size()
            + self.num_rows() * ROW_SLOT_BYTES
            + states
    }
}

/// `indices` renumbered so each distinct index gets the number `add_state`
/// returns the first time it is met (which also collects that state), and
/// every repeat the same number. Example: `[7, 3, 7]`, `add_state`
/// numbering 0, 1, … gives `[0, 1, 0]`, adding states 7 and 3.
fn remap_each_distinct(indices: &[u32], mut add_state: impl FnMut(u32) -> u32) -> Vec<u32> {
    let mut renumbered = ahash::AHashMap::<u32, u32>::new();
    indices
        .iter()
        .map(|&index| *renumbered.entry(index).or_insert_with(|| add_state(index)))
        .collect()
}

#[cfg(test)]
mod tests;
