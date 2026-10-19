use super::*;
pub(super) fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}
pub(super) fn schema(fields: Vec<SummaryField>) -> SchemaRef {
    Arc::new(Schema {
        fields,
        unique_keys: vec![],
        closed: false,
        time_index: None,
    })
}
pub(super) fn result_field(name: &str, dtype: DataType, nullable: bool) -> SummaryField {
    SummaryField {
        name: name.into(),
        dtype: SummaryFamilyType::Plain(dtype),
        nullable,
        table: None,
    }
}

pub(super) fn validate_groups(input: &SchemaRef, groups: &[usize]) -> Result<(), Error> {
    for &i in groups {
        plain(input, i)?;
    }
    if groups
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        != groups.len()
    {
        return Err(invalid("duplicate group columns"));
    }
    Ok(())
}
#[cfg(test)]
pub(super) async fn collect_rows(
    mut input: Input<'_, Batch>,
    context: &RunContext,
) -> Result<(Vec<Vec<Value>>, Vec<Reservation>), Error> {
    let mut rows = Vec::new();
    let mut work = Cooperative::new(context);
    let mut reservations = Vec::new();
    while let Some(batch) = input.next().await {
        let batch = batch?;
        reservations.push(context.reserve(batch.memory_bytes())?);
        for row in &batch.to_rows() {
            work.checkpoint().await?;
            rows.push(row.clone());
        }
    }
    Ok((rows, reservations))
}
/// Estimates retained workspace before growing collections. It is not an RSS limit.
pub(super) struct Workspace {
    reservation: Reservation,
    bytes: usize,
}
impl Workspace {
    pub(super) fn new(context: &RunContext) -> Result<Self, Error> {
        Ok(Self {
            reservation: context.reserve(0)?,
            bytes: 0,
        })
    }
    /// Reserve exactly `bytes` in total.
    pub(super) fn set_bytes(&mut self, bytes: usize) -> Result<(), Error> {
        self.bytes = bytes;
        self.reservation.resize(bytes)
    }
    pub(super) fn grow(&mut self, bytes: usize) -> Result<(), Error> {
        self.bytes = self.bytes.checked_add(bytes).ok_or(Error::MemoryLimit)?;
        self.reservation.resize(self.bytes)
    }
}
#[cfg(test)]
pub(super) fn row_bytes(row: &[Value]) -> usize {
    std::mem::size_of::<Vec<Value>>() + row.iter().map(Value::bytes).sum::<usize>()
}
#[cfg(test)]
pub(super) fn key_bytes(key: &[Vec<u8>]) -> usize {
    64 + key
        .iter()
        .map(|part| std::mem::size_of::<Vec<u8>>() + part.len())
        .sum::<usize>()
}

/// The rows a grouped operator processes at once: memory is checked after
/// each chunk's new groups.
pub(crate) const ROWS_PER_CHUNK: usize = 1024;

/// `batch` in slices of at most [`ROWS_PER_CHUNK`] rows (no copy).
pub(crate) fn row_chunks(batch: &Batch) -> impl Iterator<Item = Batch> + '_ {
    (0..batch.num_rows())
        .step_by(ROWS_PER_CHUNK)
        .map(move |offset| batch.slice(offset, ROWS_PER_CHUNK.min(batch.num_rows() - offset)))
}

/// One input's batches as one batch, for an operator that needs all its
/// input at once. A single batch stays under its producer's reservation;
/// several are copied into one, which is reserved.
pub(super) struct CollectedInput {
    /// All the input's rows.
    pub(super) batch: Batch,
    /// The producer's batch, kept with its reservation.
    _held: Option<crate::runtime::SharedValue<Batch>>,
    /// The reservation of a joined copy.
    _reservation: Option<Reservation>,
}

/// Collect `input` (of `schema`) into one batch.
pub(super) async fn collect_batch(
    mut input: Input<'_, Batch>,
    schema: &SchemaRef,
    context: &RunContext,
) -> Result<CollectedInput, Error> {
    let mut work = Cooperative::new(context);
    let mut held = Vec::new();
    while let Some(batch) = input.next().await {
        let batch = batch?;
        work.checkpoint_rows(batch.num_rows()).await?;
        held.push(batch);
    }
    if held.len() == 1 {
        let shared = held.pop().expect("one batch");
        return Ok(CollectedInput {
            batch: (*shared).clone(),
            _held: Some(shared),
            _reservation: None,
        });
    }
    let batches = held
        .iter()
        .map(|batch| (**batch).clone())
        .collect::<Vec<_>>();
    let batch = Batch::concat(schema, &batches)?;
    let reservation = context.reserve(batch.memory_bytes())?;
    drop(held);
    Ok(CollectedInput {
        batch,
        _held: None,
        _reservation: Some(reservation),
    })
}

/// Each value of plain column `column` of `batch`.
pub(super) fn decoded(batch: &Batch, column: usize) -> Result<Vec<Value>, Error> {
    let (dtype, _) = plain(batch.schema(), column)?;
    Ok(crate::values::arrow_columns::decode(
        batch.column(column).as_ref(),
        dtype,
        crate::values::arrow_columns::ColumnDepth::BatchColumn,
    ))
}

/// The group of each row of `batch` in `table`, found a chunk of rows at a
/// time, with the table's memory (plus `held_bytes`) charged to `workspace`
/// after each chunk: one large batch cannot grow the table far past the
/// limit before it is charged.
pub(super) async fn chunked_group_ids(
    table: &mut super::group_table::GroupTable,
    batch: &Batch,
    workspace: &mut Workspace,
    held_bytes: usize,
    work: &mut Cooperative,
) -> Result<Vec<u32>, Error> {
    let mut ids = Vec::with_capacity(batch.num_rows());
    for chunk in row_chunks(batch) {
        ids.extend(table.group_ids(&chunk)?);
        workspace.set_bytes(held_bytes + table.memory_bytes() + 4 * ids.capacity())?;
        work.checkpoint_rows(chunk.num_rows()).await?;
    }
    Ok(ids)
}
