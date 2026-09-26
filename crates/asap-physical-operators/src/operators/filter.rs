use super::*;
impl Operator {
    pub fn filter(input: Schema, predicate: Expression) -> Result<Self, Error> {
        if predicate.dtype(&input)?.0 != DataType::Bool {
            return Err(invalid("filter predicate must be boolean"));
        }
        Ok(Self {
            kind: Kind::Filter(predicate),
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
        Kind::Filter(predicate) => Ok(input
            .map(move |batch| {
                if context.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                let batch = batch?;
                let mut rows = Vec::new();
                for row in batch.rows() {
                    if matches!(predicate.evaluate(row)?, Value::Bool(true)) {
                        rows.push(row.clone());
                    }
                }
                Batch::try_new(output.clone(), rows)
            })
            .boxed_local()),
        _ => unreachable!(),
    }
}
