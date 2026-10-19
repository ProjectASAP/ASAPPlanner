use super::*;
impl Operator {
    pub(crate) fn evaluation_time() -> Self {
        Self {
            kind: Kind::EvaluationTime,
            inputs: vec![],
            output: schema(vec![result_field("value", DataType::Float64, false)]),
        }
    }

    pub fn source(output: SchemaRef, batches: Vec<Batch>) -> Result<Self, Error> {
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
        let output = schema(vec![result_field(
            "value",
            dtype.clone(),
            matches!(value, Value::Null),
        )]);
        Batch::try_new(output.clone(), vec![vec![value.clone()]])?;
        Ok(Self {
            kind: Kind::Constant { value, dtype },
            inputs: vec![],
            output,
        })
    }
    pub fn vector_to_scalar(input: SchemaRef, column: usize) -> Result<Self, Error> {
        if plain(&input, column)? != (&DataType::Float64, false) {
            return Err(invalid("scalar conversion requires non-null Float64"));
        }
        Ok(Self {
            kind: Kind::VectorToScalar { column },
            inputs: vec![input],
            output: schema(vec![result_field("value", DataType::Float64, false)]),
        })
    }
    pub fn union(input: SchemaRef, arity: usize) -> Result<Self, Error> {
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
    if let Kind::Constant { value, .. } = &operator.kind {
        return Ok(futures::stream::once(async move { single_value(output, value) }).boxed_local());
    }
    if matches!(operator.kind, Kind::EvaluationTime) {
        let at = match context.scope {
            crate::runtime::Scope::Query {
                evaluation_time_ms, ..
            } => evaluation_time_ms,
            crate::runtime::Scope::Ingestion { window_end_ms, .. } => window_end_ms,
        };
        return Ok(futures::stream::once(async move {
            single_value(output, &Value::Float64(at as f64 / 1000.0))
        })
        .boxed_local());
    }
    if matches!(operator.kind, Kind::Union) {
        return Ok(futures::stream::select_all(inputs)
            .map(|batch| batch.map(|batch| batch.value().clone()))
            .boxed_local());
    }
    let input = inputs.pop().ok_or_else(|| invalid("input missing"))?;
    match &operator.kind {
        Kind::VectorToScalar { column } => Ok(futures::stream::once(async move {
            // PromQL `scalar()`: the one sample's value, NaN otherwise.
            let mut input = input;
            let mut work = Cooperative::new(&context);
            let (mut count, mut value) = (0usize, f64::NAN);
            while let Some(batch) = input.next().await {
                let batch = batch?;
                work.checkpoint_rows(batch.num_rows()).await?;
                count = count.saturating_add(batch.num_rows());
                if let Some(values) = batch
                    .column(*column)
                    .as_any()
                    .downcast_ref::<arrow::array::Float64Array>()
                {
                    if let Some(last) = values.len().checked_sub(1) {
                        value = values.value(last);
                    }
                }
            }
            single_value(
                output,
                &Value::Float64(if count == 1 { value } else { f64::NAN }),
            )
        })
        .boxed_local()),
        _ => unreachable!(),
    }
}

/// The one-row batch of `output` (one column) holding `value`.
fn single_value(output: SchemaRef, value: &Value) -> Result<Batch, Error> {
    let (dtype, _) = plain(&output, 0)?;
    let array = crate::values::arrow_columns::repeat(
        dtype,
        crate::values::arrow_columns::ColumnDepth::BatchColumn,
        value,
        1,
    )?;
    Batch::from_parts(output, vec![array], vec![None], 1)
}
