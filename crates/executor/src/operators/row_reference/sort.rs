//! The row-at-a-time `Sort` (reference for tests).
use super::super::sort::{compare_rows, nested_nan};
use super::super::*;
use crate::operators::sort::cooperative_sort;

pub(super) fn execute<'a>(
    operator: &'a Operator,
    mut inputs: Vec<Input<'a, Batch>>,
    context: RunContext,
) -> Result<OutputStream<'a, Batch>, Error> {
    let output = operator.output.clone();
    let input = inputs.pop().ok_or_else(|| invalid("input missing"))?;
    Ok(futures::stream::once(async move {
        let (rows, _memory) = collect_rows(input, &context).await?;
        let result = match &operator.kind {
            Kind::Sort { keys, groups } => {
                let mut grouped = BTreeMap::<Vec<Vec<u8>>, Vec<Vec<Value>>>::new();
                let mut work = Cooperative::new(&context);
                let mut workspace = Workspace::new(&context)?;
                for row in rows {
                    work.checkpoint().await?;
                    for key in keys {
                        if matches!(row[key.column], Value::Map(_)) && nested_nan(&row[key.column])
                        {
                            return Err(invalid("NaN in collection sort key"));
                        }
                    }
                    let key = group_key(&row, groups)?;
                    workspace.grow(std::mem::size_of::<Vec<Value>>())?;
                    if !grouped.contains_key(&key) {
                        workspace.grow(key_bytes(&key))?;
                    }
                    grouped.entry(key).or_default().push(row);
                }
                let mut result = Vec::new();
                for rows in grouped.into_values() {
                    let rows =
                        cooperative_sort(rows, |a, b| compare_rows(a, b, keys), &context).await?;
                    result.extend(rows);
                }
                result
            }
            _ => unreachable!(),
        };
        Batch::try_new(output, result)
    })
    .boxed_local())
}
