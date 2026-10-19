use super::*;
impl Operator {
    pub fn project(input: SchemaRef, columns: Vec<(String, Expression)>) -> Result<Self, Error> {
        let fields = columns
            .iter()
            .map(|(name, e)| {
                if let Expression::Column(index) = e {
                    let mut field = input
                        .fields
                        .get(*index)
                        .ok_or_else(|| invalid("projection column out of range"))?
                        .clone();
                    field.name = name.clone();
                    return Ok(field);
                }
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
        // One column per expression per batch. A column reference shares the
        // input column (and its states) without copying.
        Kind::Project(expressions) => Ok(input
            .map(move |batch| {
                if context.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                let batch = batch?;
                let mut arrays = Vec::with_capacity(expressions.len());
                let mut states = Vec::with_capacity(expressions.len());
                for (expression, field) in expressions.iter().zip(&output.fields) {
                    if let Expression::Column(column) = expression {
                        arrays.push(batch.column(*column).clone());
                        states.push(batch.state_column(*column).cloned());
                        continue;
                    }
                    let SummaryFamilyType::Plain(dtype) = &field.dtype else {
                        return Err(invalid("computed projection must be plain"));
                    };
                    arrays.push(expression.evaluate_batch(&batch, dtype)?);
                    states.push(None);
                }
                Batch::from_parts(output.clone(), arrays, states, batch.num_rows())
            })
            .boxed_local()),
        _ => unreachable!(),
    }
}
