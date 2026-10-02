use super::*;
impl Operator {
    pub fn limit(input: Schema, n: u64, offset: u64, groups: Vec<usize>) -> Result<Self, Error> {
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
    let output = operator.output.clone();
    let input = inputs.pop().ok_or_else(|| invalid("input missing"))?;
    match &operator.kind {
        Kind::Limit { n, offset, groups } => {
            let counts = BTreeMap::<Vec<Vec<u8>>, u64>::new();
            Ok(futures::stream::try_unfold(
                (input, counts, Vec::<Reservation>::new(), false),
                move |(mut input, mut counts, mut memory, done)| {
                    let output = output.clone();
                    let context = context.clone();
                    async move {
                        if done || *n == 0 {
                            return Ok(None);
                        }
                        let Some(batch) = input.next().await else {
                            return Ok(None);
                        };
                        let batch = batch?;
                        let mut rows = Vec::new();
                        for row in batch.rows() {
                            let key = group_key(row, groups)?;
                            if !counts.contains_key(&key) {
                                memory.push(
                                    context.reserve(
                                        key.iter()
                                            .map(|part| part.len() + std::mem::size_of::<Vec<u8>>())
                                            .sum::<usize>()
                                            + 64,
                                    )?,
                                );
                            }
                            let count = counts.entry(key).or_default();
                            if *count >= *offset && count.saturating_sub(*offset) < *n {
                                rows.push(row.clone());
                            }
                            *count = count.saturating_add(1);
                        }
                        let done = groups.is_empty()
                            && counts
                                .get(&vec![])
                                .is_some_and(|count| count.saturating_sub(*offset) >= *n);
                        Ok(Some((
                            Batch::try_new(output, rows)?,
                            (input, counts, memory, done),
                        )))
                    }
                },
            )
            .boxed_local())
        }
        _ => unreachable!(),
    }
}
