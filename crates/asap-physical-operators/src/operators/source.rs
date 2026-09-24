use super::*;
impl Operator {
    pub fn source(output: Schema, batches: Vec<Batch>) -> Result<Self, Error> {
        crate::values::validate_schema(&output)?;
        if batches.iter().any(|b| b.schema() != &output) {
            return Err(invalid("source schema mismatch"));
        }
        Ok(Self {
            kind: Kind::Source(batches),
            inputs: vec![],
            output,
        })
    }
    pub fn scalar(value: Value, dtype: DataType) -> Result<Self, Error> {
        let schema = schema(vec![result_field(
            "value",
            dtype,
            matches!(value, Value::Null),
        )]);
        Self::source(
            schema.clone(),
            vec![Batch::try_new(schema, vec![vec![value]])?],
        )
    }
    pub fn vector_to_scalar(input: Schema, column: usize) -> Result<Self, Error> {
        if plain(&input, column)? != (&DataType::Float64, false) {
            return Err(invalid("scalar conversion requires non-null Float64"));
        }
        Ok(Self {
            kind: Kind::VectorToScalar { column },
            inputs: vec![input],
            output: schema(vec![result_field("value", DataType::Float64, false)]),
        })
    }
    pub fn union(input: Schema, arity: usize) -> Result<Self, Error> {
        if arity == 0 {
            return Err(invalid("union needs at least one input"));
        }
        Ok(Self {
            kind: Kind::Union,
            inputs: vec![input.clone(); arity],
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
    if let Kind::Source(batches) = &operator.kind {
        return Ok(futures::stream::iter(batches.iter().cloned().map(Ok)).boxed_local());
    }
    if matches!(operator.kind, Kind::Union) {
        return Ok(futures::stream::select_all(inputs)
            .map(|batch| batch.map(|batch| batch.value().clone()))
            .boxed_local());
    }
    let input = inputs.pop().ok_or_else(|| invalid("input missing"))?;
    match &operator.kind {
        Kind::VectorToScalar { column } => Ok(futures::stream::once(async move {
            let mut input = input;
            let mut work = Cooperative::new(&context);
            let mut value = f64::NAN;
            let mut count = 0usize;
            while let Some(batch) = input.next().await {
                for row in batch?.rows() {
                    work.checkpoint().await?;
                    count = count.saturating_add(1);
                    if let Value::Float64(v) = row[*column] {
                        value = v;
                    }
                }
            }
            Batch::try_new(
                output,
                vec![vec![Value::Float64(if count == 1 {
                    value
                } else {
                    f64::NAN
                })]],
            )
        })
        .boxed_local()),
        _ => unreachable!(),
    }
}
