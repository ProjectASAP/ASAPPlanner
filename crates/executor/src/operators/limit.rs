use super::*;
impl Operator {
    pub fn limit(input: SchemaRef, n: u64, offset: u64, groups: Vec<usize>) -> Result<Self, Error> {
        validate_groups(&input, &groups)?;
        Ok(Self {
            kind: Kind::Limit { n, offset, groups },
            inputs: vec![input.clone()],
            output: input,
        })
    }
}
pub(super) fn execute<'a>(
    operator: &'a Operator,
    mut inputs: Vec<Input<'a, Batch>>,
    context: RunContext,
) -> Result<OutputStream<'a, Batch>, Error> {
    let input = inputs.pop().ok_or_else(|| invalid("input missing"))?;
    let Kind::Limit { n, offset, groups } = &operator.kind else {
        unreachable!()
    };
    let (n, offset) = (*n, *offset);
    // Rows seen per group so far, across batches.
    let state = (
        input,
        super::group_table::GroupTable::new(groups.clone()),
        Vec::<u64>::new(),
        Workspace::new(&context)?,
        false,
    );
    Ok(futures::stream::try_unfold(
        state,
        move |(mut input, mut table, mut seen, mut workspace, done)| {
            let context = context.clone();
            async move {
                if done || n == 0 {
                    return Ok(None);
                }
                let Some(batch) = input.next().await else {
                    return Ok(None);
                };
                let batch = batch?;
                let mut work = Cooperative::new(&context);
                let held = seen.capacity() * std::mem::size_of::<u64>();
                let ids =
                    chunked_group_ids(&mut table, &batch, &mut workspace, held, &mut work).await?;
                seen.resize(table.len(), 0);
                workspace.set_bytes(
                    table.memory_bytes() + seen.capacity() * std::mem::size_of::<u64>(),
                )?;
                let keep = ids
                    .iter()
                    .map(|&group| {
                        let count = &mut seen[group as usize];
                        let kept = *count >= offset && count.saturating_sub(offset) < n;
                        *count = count.saturating_add(1);
                        Some(kept)
                    })
                    .collect::<arrow::array::BooleanArray>();
                let done = groups.is_empty()
                    && seen
                        .first()
                        .is_some_and(|count| count.saturating_sub(offset) >= n);
                let kept = batch.filter(&keep)?;
                Ok(Some((kept, (input, table, seen, workspace, done))))
            }
        },
    )
    .boxed_local())
}
