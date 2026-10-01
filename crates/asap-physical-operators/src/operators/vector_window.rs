//! Window bounds are typed input data; aggregation and histogram semantics stay native.
use super::*;
use planner_types::pre_asap::AggIntent;

pub(crate) fn matrix_schema() -> Schema {
    let mut fields = vector_binary::value_schema(false).fields.clone();
    fields.insert(1, result_field("timestamp", DataType::Timestamp, false));
    fields.push(result_field("window_start", DataType::Timestamp, false));
    fields.push(result_field("window_end", DataType::Timestamp, false));
    Arc::new(SummarySchema {
        fields,
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
                let (rows, _memory) = collect_rows(input, &context).await?;
                let mut window = None;
                let mut work = Cooperative::new(&context);
                for row in &rows {
                    work.checkpoint().await?;
                    let (Value::Timestamp(start), Value::Timestamp(end)) = (&row[3], &row[4])
                    else {
                        return Err(invalid("missing matrix window bounds"));
                    };
                    if start >= end || window.is_some_and(|bounds| bounds != (*start, *end)) {
                        return Err(invalid("matrix rows must share one nonempty window"));
                    }
                    window = Some((*start, *end));
                }
                let mut rows =
                    aggregate::temporal::reduce(rows, intent, &[0], 1, 2, window, &context).await?;
                for row in &mut rows {
                    work.checkpoint().await?;
                    row[1] = Expression::ExactFloat64(1).evaluate(row)?;
                }
                Batch::try_new(operator.output.clone(), rows)
            })
            .boxed_local())
        }
        Kind::HistogramQuantile => {
            let buckets = inputs
                .pop()
                .ok_or_else(|| invalid("missing histogram buckets"))?;
            let quantile = inputs.pop().ok_or_else(|| invalid("missing quantile"))?;
            Ok(futures::stream::once(async move {
                let ((quantile, _q_memory), (buckets, _bucket_memory)) = futures::try_join!(
                    collect_rows(quantile, &context),
                    collect_rows(buckets, &context)
                )?;
                let [row] = quantile.as_slice() else {
                    return Err(invalid("histogram quantile requires one scalar"));
                };
                let [Value::Float64(q)] = row.as_slice() else {
                    return Err(invalid("invalid quantile scalar"));
                };
                let mut rows = Vec::new();
                let mut work = Cooperative::new(&context);
                let mut workspace = Workspace::new(&context)?;
                for row in buckets {
                    work.checkpoint().await?;
                    let Value::Map(entries) = &row[0] else {
                        return Err(invalid("histogram buckets require labels"));
                    };
                    let mut bound = None;
                    let mut labels = BTreeMap::new();
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
                            labels.insert(key.clone(), value.clone());
                        }
                    }
                    if let Some(bound) = bound {
                        let projected = vec![
                            Value::Map(
                                labels
                                    .into_iter()
                                    .map(|(k, v)| (Value::Utf8(k), Value::Utf8(v)))
                                    .collect::<Vec<_>>()
                                    .into(),
                            ),
                            Value::Float64(bound),
                            row[1].clone(),
                        ];
                        workspace.grow(row_bytes(&projected))?;
                        rows.push(projected);
                    }
                }
                let result = aggregate::temporal::reduce(
                    rows,
                    &AggIntent::HistogramQuantile { q: *q },
                    &[0],
                    1,
                    2,
                    None,
                    &context,
                )
                .await?;
                Batch::try_new(operator.output.clone(), result)
            })
            .boxed_local())
        }
        _ => unreachable!(),
    }
}
