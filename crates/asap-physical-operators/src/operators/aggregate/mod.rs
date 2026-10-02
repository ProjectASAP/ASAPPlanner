use super::*;
impl Operator {
    pub fn aggregate(
        input: Schema,
        groups: Vec<usize>,
        measures: Vec<(String, Reduction)>,
    ) -> Result<Self, Error> {
        validate_groups(&input, &groups)?;
        let mut fields = groups
            .iter()
            .map(|&i| input.fields[i].clone())
            .collect::<Vec<_>>();
        for (name, reduction) in &measures {
            let (t, n) = match reduction {
                Reduction::Count => (DataType::Int64, false),
                Reduction::Sum(i) | Reduction::Avg(i) => {
                    let (t, _) = plain(&input, *i)?;
                    if !matches!(t, DataType::Int64 | DataType::Float64) {
                        return Err(invalid("numeric aggregate input required"));
                    }
                    (
                        if matches!(reduction, Reduction::Avg(_)) {
                            DataType::Float64
                        } else {
                            t.clone()
                        },
                        false,
                    )
                }
                Reduction::Quantile { column, q } => {
                    if plain(&input, *column)?.0 != &DataType::Float64 || q.is_nan() {
                        return Err(invalid("quantile requires Float64 input and a numeric q"));
                    }
                    (DataType::Float64, false)
                }
                Reduction::Min(i) | Reduction::Max(i) => {
                    let (t, nullable) = plain(&input, *i)?;
                    if !ordered(t) {
                        return Err(invalid("ordered aggregate input required"));
                    }
                    (t.clone(), nullable || groups.is_empty())
                }
            };
            fields.push(result_field(name, t, n));
        }
        Ok(Self {
            kind: Kind::Aggregate {
                groups,
                measures: measures.into_iter().map(|(_, r)| r).collect(),
            },
            inputs: vec![input],
            output: schema(fields),
        })
    }
    pub fn window(
        input: Schema,
        intent: planner_types::pre_asap::AggIntent<ColumnRef>,
        coordinate: usize,
        value: usize,
        groups: Vec<usize>,
        window: Option<(i64, i64)>,
    ) -> Result<Self, Error> {
        use planner_types::pre_asap::AggIntent;
        validate_groups(&input, &groups)?;
        let histogram = matches!(intent, AggIntent::HistogramQuantile { .. });
        if !matches!(
            intent,
            AggIntent::Rate
                | AggIntent::Deriv
                | AggIntent::PredictLinear { .. }
                | AggIntent::Increase
                | AggIntent::Count { .. }
                | AggIntent::Sum { col: None }
                | AggIntent::Avg { col: None }
                | AggIntent::Min { col: None }
                | AggIntent::Max { col: None }
                | AggIntent::HistogramQuantile { .. }
        ) {
            return Err(invalid(
                "unsupported temporal intent or unresolved value column",
            ));
        }
        let coordinate_type = if histogram {
            DataType::Float64
        } else {
            DataType::Timestamp
        };
        if plain(&input, coordinate)? != (&coordinate_type, false)
            || plain(&input, value)? != (&DataType::Float64, false)
        {
            return Err(invalid("window coordinate/value schema mismatch"));
        }
        if (!histogram && !matches!(window, Some((start, end)) if start < end))
            || (histogram && window.is_some())
        {
            return Err(invalid("invalid temporal window"));
        }
        let mut fields = groups
            .iter()
            .map(|i| input.fields[*i].clone())
            .collect::<Vec<_>>();
        fields.push(result_field(
            "value",
            if matches!(intent, AggIntent::Count { .. }) {
                DataType::Int64
            } else {
                DataType::Float64
            },
            false,
        ));
        Ok(Self {
            kind: Kind::Window {
                intent: Box::new(intent),
                coordinate,
                value,
                groups,
                window,
            },
            inputs: vec![input],
            output: schema(fields),
        })
    }
}
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub enum Reduction {
    Count,
    Sum(usize),
    Avg(usize),
    Min(usize),
    Max(usize),
    /// PromQL `quantile`: linear interpolation between closest ranks.
    Quantile {
        column: usize,
        q: f64,
    },
}
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
            Kind::Window {
                intent,
                coordinate,
                value,
                groups,
                window,
            } => {
                crate::operators::aggregate::temporal::reduce(
                    rows,
                    intent,
                    groups,
                    *coordinate,
                    *value,
                    *window,
                    &context,
                )
                .await?
            }
            Kind::Aggregate { groups, measures } => {
                reduce(rows, groups, measures, &operator.inputs[0], &context).await?
            }
            _ => unreachable!(),
        };
        Batch::try_new(output, result)
    })
    .boxed_local())
}

pub(super) mod temporal;
async fn reduce(
    rows: Vec<Vec<Value>>,
    groups: &[usize],
    measures: &[Reduction],
    input: &Schema,
    context: &RunContext,
) -> Result<Vec<Vec<Value>>, Error> {
    let mut work = Cooperative::new(context);
    let mut workspace = Workspace::new(context)?;
    let mut grouped = BTreeMap::<Vec<Vec<u8>>, Vec<Vec<Value>>>::new();
    // PromQL sums over an empty vector emit no sample.
    if rows.is_empty() && groups.is_empty() && !input.has_promql_series_identity() {
        grouped.insert(vec![], vec![]);
    }
    for row in rows {
        work.checkpoint().await?;
        let key = group_key(&row, groups)?;
        workspace.grow(std::mem::size_of::<Vec<Value>>())?;
        if !grouped.contains_key(&key) {
            workspace.grow(key_bytes(&key))?;
        }
        grouped.entry(key).or_default().push(row);
    }
    let mut output = Vec::new();
    for rows in grouped.into_values() {
        work.checkpoint().await?;
        let mut result = groups
            .iter()
            .map(|&i| rows[0][i].clone())
            .collect::<Vec<_>>();
        for measure in measures {
            result.push(reduce_one(&rows, measure, input, &mut work).await?);
        }
        workspace.grow(row_bytes(&result))?;
        output.push(result);
    }
    Ok(output)
}

// Matches Prometheus `quantile`: NaN for no values, ±Inf outside [0, 1],
// and NaN samples ordered first.
pub(super) fn quantile(q: f64, mut values: Vec<f64>) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    if q < 0. {
        return f64::NEG_INFINITY;
    }
    if q > 1. {
        return f64::INFINITY;
    }
    values.sort_by(|a, b| match (a.is_nan(), b.is_nan()) {
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.total_cmp(b),
    });
    let rank = q * (values.len() - 1) as f64;
    let low = rank.floor() as usize;
    let high = (low + 1).min(values.len() - 1);
    let weight = rank - low as f64;
    values[low] * (1. - weight) + values[high] * weight
}

async fn reduce_one(
    rows: &[Vec<Value>],
    measure: &Reduction,
    input: &Schema,
    work: &mut Cooperative,
) -> Result<Value, Error> {
    let column = match measure {
        Reduction::Count => {
            return Ok(Value::Int64(
                i64::try_from(rows.len()).map_err(|_| invalid("count overflow"))?,
            ))
        }
        Reduction::Sum(i) | Reduction::Avg(i) | Reduction::Min(i) | Reduction::Max(i) => *i,
        Reduction::Quantile { column, q } => {
            let mut values = Vec::with_capacity(rows.len());
            for row in rows {
                work.checkpoint().await?;
                match &row[*column] {
                    Value::Float64(value) => values.push(*value),
                    Value::Null => {}
                    _ => return Err(invalid("floating quantile value required")),
                }
            }
            return Ok(Value::Float64(quantile(*q, values)));
        }
    };
    let values = rows
        .iter()
        .map(|r| &r[column])
        .filter(|v| !matches!(v, Value::Null));
    if matches!(measure, Reduction::Min(_) | Reduction::Max(_)) {
        if plain(input, column)?.0 == &DataType::Float64 {
            // Match exact-state kernels: ignore NaN when a numeric value exists.
            let mut best: Option<f64> = None;
            for value in values {
                work.checkpoint().await?;
                let Value::Float64(value) = value else {
                    return Err(invalid("floating aggregate value required"));
                };
                best = Some(best.map_or(*value, |old| {
                    if matches!(measure, Reduction::Min(_)) {
                        old.min(*value)
                    } else {
                        old.max(*value)
                    }
                }));
            }
            return Ok(best.map(Value::Float64).unwrap_or(Value::Null));
        }
        let mut best: Option<&Value> = None;
        for value in values {
            work.checkpoint().await?;
            if best
                .map(|b| value.compare(b))
                .transpose()?
                .is_none_or(|order| {
                    if matches!(measure, Reduction::Min(_)) {
                        order.is_lt()
                    } else {
                        order.is_gt()
                    }
                })
            {
                best = Some(value);
            }
        }
        return Ok(best.cloned().unwrap_or(Value::Null));
    }
    let dtype = plain(input, column)?.0;
    if dtype == &DataType::Int64 {
        let mut count = 0usize;
        let mut sum = 0i128;
        for v in values {
            work.checkpoint().await?;
            let Value::Int64(v) = v else {
                return Err(invalid("integer aggregate value required"));
            };
            sum = sum
                .checked_add(i128::from(*v))
                .ok_or_else(|| invalid("integer aggregate overflow"))?;
            count += 1;
        }
        return if matches!(measure, Reduction::Avg(_)) {
            Ok(Value::Float64(sum as f64 / count as f64))
        } else {
            Ok(Value::Int64(
                i64::try_from(sum).map_err(|_| invalid("integer sum overflow"))?,
            ))
        };
    }
    let mut floats = Vec::with_capacity(rows.len());
    for v in values {
        work.checkpoint().await?;
        let Value::Float64(v) = v else {
            return Err(invalid("floating aggregate value required"));
        };
        floats.push(*v);
    }
    Ok(Value::Float64(if matches!(measure, Reduction::Avg(_)) {
        promql_avg(&floats)
    } else {
        // Prometheus starts `sum` from the first value; adding it to 0 gives
        // the same sum and compensation.
        promql_sum(0., &floats)
    }))
}

/// Prometheus `kahansum.Inc`: Kahan-Neumaier compensated addition, with the
/// compensation cleared once the sum is infinite.
fn kahan_inc(inc: f64, sum: f64, c: f64) -> (f64, f64) {
    let t = sum + inc;
    let c = if t.is_infinite() {
        0.
    } else if sum.abs() >= inc.abs() {
        c + ((sum - t) + inc)
    } else {
        c + ((inc - t) + sum)
    };
    (t, c)
}

/// Prometheus `sum` and `sum_over_time` from `start`.
pub(in crate::operators) fn promql_sum(start: f64, values: &[f64]) -> f64 {
    let (sum, c) = values
        .iter()
        .fold((start, 0.), |(sum, c), &v| kahan_inc(v, sum, c));
    if sum.is_infinite() {
        sum
    } else {
        sum + c
    }
}

/// Prometheus `avg` and `avg_over_time`: a compensated sum divided by the
/// count until the running sum would overflow, then an incremental mean.
/// NaN for no values.
pub(in crate::operators) fn promql_avg(values: &[f64]) -> f64 {
    let Some((&first, rest)) = values.split_first() else {
        return f64::NAN;
    };
    let (mut sum, mut c, mut mean, mut incremental) = (first, 0., 0., false);
    for (i, &v) in rest.iter().enumerate() {
        let count = (i + 2) as f64;
        if !incremental {
            let (next, next_c) = kahan_inc(v, sum, c);
            if !next.is_infinite() {
                (sum, c) = (next, next_c);
                continue;
            }
            incremental = true;
            mean = sum / (count - 1.);
            c /= count - 1.;
        }
        let q = (count - 1.) / count;
        (mean, c) = kahan_inc(v / count, q * mean, q * c);
    }
    let count = values.len() as f64;
    if incremental {
        mean + c
    } else {
        sum / count + c / count
    }
}

#[cfg(test)]
mod tests {
    // Quantile follows Prometheus: interpolate ranks, NaN when empty, ±Inf outside [0, 1].
    #[test]
    fn quantile_matches_prometheus_edge_cases() {
        assert!(super::quantile(0.5, vec![]).is_nan());
        assert_eq!(super::quantile(0.5, vec![3.]), 3.);
        assert_eq!(super::quantile(0.75, vec![4., 1., 2., 3.]), 3.25);
        assert_eq!(super::quantile(-0.1, vec![1.]), f64::NEG_INFINITY);
        assert_eq!(super::quantile(1.1, vec![1.]), f64::INFINITY);
        assert_eq!(super::quantile(1., vec![2., f64::NAN, 1.]), 2.);
    }
}
