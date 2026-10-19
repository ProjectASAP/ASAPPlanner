//! The row-at-a-time `Window` aggregate (reference for tests).
use super::super::*;

pub(super) fn execute<'a>(
    operator: &'a Operator,
    mut inputs: Vec<Input<'a, Batch>>,
    context: RunContext,
) -> Result<OutputStream<'a, Batch>, Error> {
    let output = operator.output.clone();
    let input = inputs.pop().ok_or_else(|| invalid("input missing"))?;
    Ok(futures::stream::once(async move {
        let Kind::Window {
            intent,
            coordinate,
            value,
            groups,
            window,
        } = &operator.kind
        else {
            unreachable!()
        };
        let (rows, _memory) = collect_rows(input, &context).await?;
        let result = crate::operators::aggregate::temporal::reduce(
            rows,
            intent,
            groups,
            *coordinate,
            *value,
            *window,
            &context,
        )
        .await?;
        Batch::try_new(output, result)
    })
    .boxed_local())
}
