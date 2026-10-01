use super::*;
pub(super) fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}
pub(super) fn schema(fields: Vec<SummaryField>) -> Schema {
    Arc::new(SummarySchema {
        fields,
        time_index: None,
    })
}
pub(super) fn result_field(name: &str, dtype: DataType, nullable: bool) -> SummaryField {
    SummaryField {
        name: name.into(),
        dtype: SummaryFamilyType::Plain(dtype),
        nullable,
    }
}

pub(super) fn validate_groups(input: &Schema, groups: &[usize]) -> Result<(), Error> {
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
pub(super) async fn collect_rows(
    mut input: Input<'_, Batch>,
    context: &RunContext,
) -> Result<(Vec<Vec<Value>>, Vec<Reservation>), Error> {
    let mut rows = Vec::new();
    let mut work = Cooperative::new(context);
    let mut reservations = Vec::new();
    while let Some(batch) = input.next().await {
        let batch = batch?;
        reservations.push(context.reserve(batch.bytes())?);
        for row in batch.rows() {
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
    pub(super) fn grow(&mut self, bytes: usize) -> Result<(), Error> {
        self.bytes = self.bytes.checked_add(bytes).ok_or(Error::MemoryLimit)?;
        self.reservation.resize(self.bytes)
    }
}
pub(super) fn row_bytes(row: &[Value]) -> usize {
    std::mem::size_of::<Vec<Value>>() + row.iter().map(Value::bytes).sum::<usize>()
}
pub(super) fn key_bytes(key: &[Vec<u8>]) -> usize {
    64 + key
        .iter()
        .map(|part| std::mem::size_of::<Vec<u8>>() + part.len())
        .sum::<usize>()
}
