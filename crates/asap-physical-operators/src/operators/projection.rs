use super::*;
impl Operator {
    pub fn project(input: Schema, columns: Vec<(String, Expression)>) -> Result<Self, Error> {
        let fields = columns
            .iter()
            .map(|(name, e)| {
                let (t, n) = e.dtype(&input)?;
                Ok(result_field(name, t, n))
            })
            .collect::<Result<_, Error>>()?;
        Ok(Self {
            kind: Kind::Project(columns.into_iter().map(|(_, e)| e).collect()),
            inputs: vec![input],
            output: schema(fields),
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
        Kind::Project(expressions) => Ok(input
            .map(move |batch| {
                if context.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                let batch = batch?;
                let rows = batch
                    .rows()
                    .iter()
                    .map(|r| {
                        expressions
                            .iter()
                            .map(|e| e.evaluate(r))
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Batch::try_new(output.clone(), rows)
            })
            .boxed_local()),
        _ => unreachable!(),
    }
}
