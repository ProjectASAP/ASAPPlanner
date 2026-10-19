use super::*;
impl Operator {
    pub fn aggregate(
        input: SchemaRef,
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
                Reduction::Cardinality(columns) => {
                    if columns.is_empty() {
                        return Err(invalid(
                            "distinct aggregate requires at least one identity column",
                        ));
                    }
                    for column in columns {
                        plain(&input, *column)?;
                    }
                    (DataType::Int64, false)
                }
                Reduction::FrequencyL2(i) | Reduction::FrequencyEntropy(i) => {
                    if !matches!(
                        plain(&input, *i)?.0,
                        DataType::Bool | DataType::Int64 | DataType::Float64 | DataType::Utf8
                    ) {
                        return Err(invalid(
                            "frequency aggregate requires a Boolean, Int64, Float64 or Utf8 identity",
                        ));
                    }
                    (DataType::Float64, false)
                }
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
                        !input.has_promql_series_identity()
                            && (groups.is_empty() || plain(&input, *i)?.1),
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
                filters: vec![],
            },
            inputs: vec![input],
            output: schema(fields),
        })
    }

    /// Give each measure an optional row filter (SQL `FILTER (WHERE …)`).
    /// A group is formed from all its rows, so a group with no matching row
    /// still appears, with the measure's empty-input value: 0 for counts,
    /// NULL for SQL SUM/AVG/MIN/MAX. No filter at all keeps the plain form.
    pub fn with_measure_filters(mut self, filters: Vec<Option<Expression>>) -> Result<Self, Error> {
        let Kind::Aggregate {
            filters: slot,
            measures,
            groups,
        } = &mut self.kind
        else {
            return Err(invalid("measure filters need an aggregate"));
        };
        if filters.iter().all(Option::is_none) {
            return Ok(self);
        }
        if filters.len() != measures.len() {
            return Err(invalid("one filter slot per aggregate measure required"));
        }
        // PromQL emits no per-measure filters (label matchers are scan
        // predicates), and its outputs are non-nullable.
        if self.inputs[0].has_promql_series_identity() {
            return Err(invalid(
                "measure filters are not supported over PromQL series identity",
            ));
        }
        let mut output = (*self.output).clone();
        for (index, filter) in filters.iter().enumerate() {
            let Some(filter) = filter else { continue };
            if filter.dtype(&self.inputs[0])?.0 != DataType::Bool {
                return Err(invalid("measure filter must be boolean"));
            }
            if matches!(
                measures[index],
                Reduction::Sum(_) | Reduction::Avg(_) | Reduction::Min(_) | Reduction::Max(_)
            ) {
                output.fields[groups.len() + index].nullable = true;
            }
        }
        *slot = filters;
        self.output = Arc::new(output);
        Ok(self)
    }
    pub fn window(
        input: SchemaRef,
        intent: planner_types::ir::operator::AggIntent<ColumnRef>,
        coordinate: usize,
        value: usize,
        groups: Vec<usize>,
        window: Option<(i64, i64)>,
    ) -> Result<Self, Error> {
        use planner_types::ir::operator::AggIntent;
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
    /// Exact distinct tuple count; a tuple with any NULL component is skipped.
    Cardinality(Vec<usize>),
    /// L2 norm of the unit-update frequencies of one identity column; NULL
    /// identities are skipped.
    FrequencyL2(usize),
    /// Shannon entropy in bits of the frequencies of one identity column;
    /// NULL identities are skipped.
    FrequencyEntropy(usize),
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
        match &operator.kind {
            Kind::Window {
                intent,
                coordinate,
                value,
                groups,
                window,
            } => {
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
            }
            Kind::Aggregate {
                groups,
                measures,
                filters,
            } => {
                columnar::aggregate(
                    input,
                    groups,
                    measures,
                    filters,
                    &operator.inputs[0],
                    &output,
                    &context,
                )
                .await
            }
            _ => unreachable!(),
        }
    })
    .boxed_local())
}

mod columnar;
#[cfg(test)]
mod row_reference;
pub(super) mod temporal;
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

/// Prometheus `kahansum.Inc`: Kahan-Neumaier compensated addition, with the
/// compensation cleared once the sum is infinite.
pub(super) fn kahan_inc(inc: f64, sum: f64, c: f64) -> (f64, f64) {
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
