//! Window bounds are typed input data; aggregation and histogram semantics stay native.
use super::*;
use planner_types::ir::operator::AggIntent;

pub(crate) fn matrix_schema() -> SchemaRef {
    let mut fields = vector_binary::value_schema(false).fields.clone();
    fields.insert(1, result_field("timestamp", DataType::Timestamp, false));
    fields.push(result_field("window_start", DataType::Timestamp, false));
    fields.push(result_field("window_end", DataType::Timestamp, false));
    Arc::new(Schema {
        fields,
        unique_keys: vec![],
        closed: false,
        time_index: Some(1),
    })
}

impl Operator {
    pub fn range_window(intent: AggIntent<ColumnRef>) -> Result<Self, Error> {
        // Reuse the window constructor's semantic admission, without fixing request time.
        Self::window(matrix_schema(), intent.clone(), 1, 2, vec![0], Some((0, 1)))?;
        Ok(Self {
            kind: Kind::RangeWindow {
                intent: Box::new(intent),
            },
            inputs: vec![matrix_schema()],
            output: vector_binary::value_schema(false),
        })
    }
    pub fn histogram_quantile() -> Self {
        Self {
            kind: Kind::HistogramQuantile,
            inputs: vec![
                vector_binary::value_schema(true),
                vector_binary::value_schema(false),
            ],
            output: vector_binary::value_schema(false),
        }
    }
}

pub(super) fn execute<'a>(
    operator: &'a Operator,
    mut inputs: Vec<Input<'a, Batch>>,
    context: RunContext,
) -> Result<OutputStream<'a, Batch>, Error> {
    match &operator.kind {
        Kind::RangeWindow { intent } => {
            let input = inputs
                .pop()
                .ok_or_else(|| invalid("missing matrix input"))?;
            Ok(futures::stream::once(async move {
                let input = collect_batch(input, &operator.inputs[0], &context).await?;
                let batch = &input.batch;
                // Every row shares one nonempty window.
                let bounds = |column: usize| {
                    batch
                        .column(column)
                        .as_any()
                        .downcast_ref::<arrow::array::TimestampMillisecondArray>()
                        .cloned()
                        .ok_or_else(|| invalid("missing matrix window bounds"))
                };
                let (starts, ends) = (bounds(3)?, bounds(4)?);
                let mut window = None;
                for row in 0..batch.num_rows() {
                    let (start, end) = (starts.value(row), ends.value(row));
                    if start >= end || window.is_some_and(|bounds| bounds != (start, end)) {
                        return Err(invalid("matrix rows must share one nonempty window"));
                    }
                    window = Some((start, end));
                }
                let (mut arrays, results) =
                    aggregate::temporal::reduce_batch(batch, intent, &[0], 1, 2, window, &context)
                        .await?;
                // The result as an exact Float64.
                let values = results
                    .iter()
                    .map(|result| {
                        Expression::ExactFloat64(0).evaluate(std::slice::from_ref(result))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                arrays.push(crate::values::arrow_columns::encode(
                    &DataType::Float64,
                    crate::values::arrow_columns::ColumnDepth::BatchColumn,
                    &values.iter().collect::<Vec<_>>(),
                )?);
                Batch::from_parts(operator.output.clone(), arrays, vec![None; 2], values.len())
            })
            .boxed_local())
        }
        Kind::HistogramQuantile => {
            let buckets = inputs
                .pop()
                .ok_or_else(|| invalid("missing histogram buckets"))?;
            let quantile = inputs.pop().ok_or_else(|| invalid("missing quantile"))?;
            Ok(futures::stream::once(async move {
                let (quantile, buckets) = futures::try_join!(
                    collect_batch(quantile, &operator.inputs[0], &context),
                    collect_batch(buckets, &operator.inputs[1], &context)
                )?;
                let quantile = decoded(&quantile.batch, 0)?;
                let [Value::Float64(q)] = quantile.as_slice() else {
                    return Err(invalid(if quantile.len() == 1 {
                        "invalid quantile scalar"
                    } else {
                        "histogram quantile requires one scalar"
                    }));
                };
                let batch = &buckets.batch;
                let label_sets = decoded(batch, 0)?;
                let counts = decoded(batch, 1)?;
                // Each bucket row with an `le`: its labels without `le` and
                // `__name__`, its bound and its count.
                let (mut labels_out, mut bounds_out, mut counts_out) =
                    (Vec::new(), Vec::new(), Vec::new());
                for (labels, count) in label_sets.into_iter().zip(counts) {
                    let Value::Map(entries) = &labels else {
                        return Err(invalid("histogram buckets require labels"));
                    };
                    let mut bound = None;
                    let mut kept = BTreeMap::new();
                    let mut seen = std::collections::BTreeSet::new();
                    for (key, value) in entries.iter() {
                        let (Value::Utf8(key), Value::Utf8(value)) = (key, value) else {
                            return Err(invalid("histogram labels must be Utf8"));
                        };
                        if !seen.insert(key) {
                            return Err(invalid("duplicate histogram label"));
                        }
                        if key.as_ref() == "le" {
                            bound = value.parse::<f64>().ok();
                        } else if key.as_ref() != "__name__" && !value.is_empty() {
                            kept.insert(key.clone(), value.clone());
                        }
                    }
                    if let Some(bound) = bound {
                        labels_out.push(Value::Map(
                            kept.into_iter()
                                .map(|(k, v)| (Value::Utf8(k), Value::Utf8(v)))
                                .collect::<Vec<_>>()
                                .into(),
                        ));
                        bounds_out.push(bound);
                        counts_out.push(count);
                    }
                }
                let projected_schema = schema(vec![
                    vector_binary::value_schema(false).fields[0].clone(),
                    result_field("le", DataType::Float64, false),
                    result_field("value", DataType::Float64, false),
                ]);
                let projected = Batch::from_parts(
                    projected_schema.clone(),
                    vec![
                        crate::values::arrow_columns::encode(
                            &projected_schema.fields[0]
                                .plain_dtype()
                                .cloned()
                                .expect("plain"),
                            crate::values::arrow_columns::ColumnDepth::BatchColumn,
                            &labels_out.iter().collect::<Vec<_>>(),
                        )?,
                        Arc::new(arrow::array::Float64Array::from(bounds_out)),
                        crate::values::arrow_columns::encode(
                            &DataType::Float64,
                            crate::values::arrow_columns::ColumnDepth::BatchColumn,
                            &counts_out.iter().collect::<Vec<_>>(),
                        )?,
                    ],
                    vec![None; 3],
                    labels_out.len(),
                )?;
                let mut workspace = Workspace::new(&context)?;
                workspace.grow(projected.memory_bytes())?;
                let (mut arrays, results) = aggregate::temporal::reduce_batch(
                    &projected,
                    &AggIntent::HistogramQuantile {
                        q: *q,
                        le: ColumnRef::Named("le".into()),
                    },
                    &[0],
                    1,
                    2,
                    None,
                    &context,
                )
                .await?;
                arrays.push(crate::values::arrow_columns::encode(
                    &DataType::Float64,
                    crate::values::arrow_columns::ColumnDepth::BatchColumn,
                    &results.iter().collect::<Vec<_>>(),
                )?);
                Batch::from_parts(
                    operator.output.clone(),
                    arrays,
                    vec![None; 2],
                    results.len(),
                )
            })
            .boxed_local())
        }
        _ => unreachable!(),
    }
}
