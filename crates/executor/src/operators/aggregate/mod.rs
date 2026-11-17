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
                // NULL for a group of NULLs, as SQL `SUM` (and for a
                // global one over no row: `with_emit_empty_global`).
                Reduction::Sum(i) | Reduction::Avg(i) => {
                    let (t, nullable) = plain(&input, *i)?;
                    if !matches!(t, DataType::Int64 | DataType::Float64) {
                        return Err(invalid("numeric aggregate input required"));
                    }
                    (
                        if matches!(reduction, Reduction::Avg(_)) {
                            DataType::Float64
                        } else {
                            t.clone()
                        },
                        nullable,
                    )
                }
                Reduction::Quantile { column, .. } => {
                    // A NaN q is allowed: Prometheus answers it with NaN.
                    if plain(&input, *column)?.0 != &DataType::Float64 {
                        return Err(invalid("quantile requires Float64 input"));
                    }
                    (DataType::Float64, false)
                }
                Reduction::Percentile { column, q } => {
                    if plain(&input, *column)?.0 != &DataType::Float64 || !(0.0..=1.0).contains(q) {
                        return Err(invalid(
                            "a percentile requires Float64 input and a fraction in [0, 1]",
                        ));
                    }
                    // NULL for a group of NULLs, as SQL `SUM` (and for a
                    // global one over no row: `with_emit_empty_global`).
                    (DataType::Float64, plain(&input, *column)?.1)
                }
                Reduction::Min(i) | Reduction::Max(i) => {
                    let (t, nullable) = plain(&input, *i)?;
                    if !ordered(t) {
                        return Err(invalid("ordered aggregate input required"));
                    }
                    (t.clone(), nullable)
                }
                Reduction::StdDev { column, population }
                | Reduction::Variance { column, population } => {
                    let (t, nullable) = plain(&input, *column)?;
                    if !matches!(t, DataType::Int64 | DataType::Float64) {
                        return Err(invalid("numeric aggregate input required"));
                    }
                    // NULL for a group of NULLs and, as SQL's sample
                    // `STDDEV`/`VAR_SAMP`, for a group of one value (and for
                    // a global one over no row: `with_emit_empty_global`).
                    // PromQL's `stddev`/`stdvar` are population ones over
                    // groups that have a value, so never NULL. The IR's
                    // aggregate schema defines this nullability; this
                    // derives it again to check the declared output.
                    (DataType::Float64, !population || nullable)
                }
                Reduction::PearsonCorr { left, right } => {
                    for column in [left, right] {
                        if !matches!(
                            plain(&input, *column)?.0,
                            DataType::Int64 | DataType::Float64
                        ) {
                            return Err(invalid("correlation requires numeric inputs"));
                        }
                    }
                    (DataType::Float64, true)
                }
                Reduction::Group => (DataType::Float64, false),
            };
            fields.push(result_field(name, t, n));
        }
        Ok(Self {
            kind: Kind::Aggregate {
                groups,
                measures: measures.into_iter().map(|(_, r)| r).collect(),
                filters: vec![],
                emit_empty_global: false,
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
            ..
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
            if measures[index].is_null_without_values() {
                output.fields[groups.len() + index].nullable = true;
            }
        }
        *slot = filters;
        self.output = Arc::new(output);
        Ok(self)
    }
    /// Each group's `intent` over its samples. `window` is the samples'
    /// time window back from the evaluation time (an ingestion run's window
    /// end): `rate(x[5m] offset 1m)` reads `TimeWindow {
    /// ends_before_evaluation_ms: 60_000, length_ms: 300_000 }`. `None` only
    /// for a histogram quantile, whose coordinate is a bucket bound.
    pub fn window(
        input: SchemaRef,
        intent: planner_types::ir::operator::AggIntent,
        coordinate: usize,
        value: usize,
        groups: Vec<usize>,
        window: Option<planner_types::plan::TimeWindow>,
    ) -> Result<Self, Error> {
        use planner_types::ir::operator::AggIntent;
        validate_groups(&input, &groups)?;
        let histogram = matches!(intent, AggIntent::HistogramQuantile { .. });
        if !histogram && !super::range_function::has_range_function_kernel(&intent) {
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
        if (!histogram && !matches!(window, Some(window) if window.length_ms > 0))
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
            if matches!(intent, AggIntent::Count) {
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
        /// NaN and ±Inf survive a JSON round trip of the compiled plan.
        #[serde(with = "planner_types::serde_f64")]
        q: f64,
    },
    /// SQL `percentile_cont` (the exact form of `approx_percentile_cont`):
    /// NULLs ignored, NaN ordered after every number, linear interpolation
    /// between closest ranks, NULL for a group with no value.
    Percentile {
        column: usize,
        /// The fraction, in [0, 1].
        q: f64,
    },
    /// Standard deviation of a numeric column's non-NULL values: the
    /// square root of [`Reduction::Variance`].
    StdDev {
        column: usize,
        population: bool,
    },
    /// Variance of a numeric column's non-NULL values, by Welford's
    /// update as Prometheus `stdvar` and DataFusion `var_pop`/`var_samp`
    /// compute it: the squared deviations' sum over n (population) or
    /// n - 1 (sample); NaN once a value is NaN or infinite. SQL gives NULL
    /// for no value, or one value of a sample; PromQL NaN. The values 1, 2
    /// and 4 have mean 7/3: population variance 14/9, sample variance 7/3.
    Variance {
        column: usize,
        population: bool,
    },
    /// SQL `corr(left, right)`, DataFusion's: Pearson's correlation over
    /// the rows where both are non-NULL; NULL for fewer than two rows, a
    /// constant input or one input's mean NaN, NaN when both are.
    PearsonCorr {
        left: usize,
        right: usize,
    },
    /// PromQL `group`: 1 for every group.
    Group,
}
impl Reduction {
    /// Whether the measure is NULL for a group with no (matching) value, as
    /// SQL `SELECT sum(x) FROM t WHERE false` is NULL while `count(*)` is 0.
    pub(super) fn is_null_without_values(&self) -> bool {
        matches!(
            self,
            Reduction::Sum(_)
                | Reduction::Avg(_)
                | Reduction::Min(_)
                | Reduction::Max(_)
                | Reduction::Percentile { .. }
                | Reduction::StdDev { .. }
                | Reduction::Variance { .. }
        )
    }
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
                let input = collect_batch(input, &operator.inputs[0], &context).await?;
                let window = window
                    .map(|window| {
                        crate::operators::time_range::time_window_bounds(window, &context)
                    })
                    .transpose()?;
                let (mut arrays, results) = super::range_function::reduce_batch(
                    &input.batch,
                    intent,
                    groups,
                    *coordinate,
                    *value,
                    window,
                    &context,
                )
                .await?;
                let (dtype, _) = plain(&output, output.fields.len() - 1)?;
                arrays.push(crate::values::arrow_columns::encode(
                    dtype,
                    crate::values::arrow_columns::ColumnDepth::BatchColumn,
                    &results.iter().collect::<Vec<_>>(),
                )?);
                Batch::from_parts(
                    output.clone(),
                    arrays,
                    vec![None; output.fields.len()],
                    results.len(),
                )
            }
            Kind::Aggregate { .. } => columnar::aggregate(input, operator, &context).await,
            _ => unreachable!(),
        }
    })
    .boxed_local())
}

mod columnar;
// Matches Prometheus `quantile`: NaN for no values or a NaN q, ±Inf outside
// [0, 1], and NaN samples ordered first.
pub(in crate::operators) fn quantile(q: f64, mut values: Vec<f64>) -> f64 {
    if values.is_empty() || q.is_nan() {
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
    interpolated(q, &values)
}

/// SQL `percentile_cont` of a group's non-NULL `values` at `q` in [0, 1]:
/// NaN ordered after every number (DataFusion's order), `None` when empty.
pub(super) fn percentile_cont(q: f64, mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| match (a.is_nan(), b.is_nan()) {
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        _ => a.total_cmp(b),
    });
    Some(interpolated(q, &values))
}

/// Linear interpolation between the closest ranks of nonempty, sorted
/// `values` at rank `q * (n - 1)`, as Prometheus `quantile` and SQL
/// `percentile_cont` both define it.
fn interpolated(q: f64, values: &[f64]) -> f64 {
    let rank = q * (values.len() - 1) as f64;
    let low = rank.floor() as usize;
    let high = (low + 1).min(values.len() - 1);
    let weight = rank - low as f64;
    // A rank on a value reads it alone, as Prometheus does: weighing the
    // next value by 0 would make an infinite neighbor NaN (1, 2, +Inf at
    // q = 0.5 is 2).
    if weight == 0. {
        return values[low];
    }
    values[low] * (1. - weight) + values[high] * weight
}

/// Prometheus `kahansum.Inc`: Kahan-Neumaier compensated addition, with the
/// compensation cleared once the sum is infinite.
pub(in crate::operators) fn kahan_inc(inc: f64, sum: f64, c: f64) -> (f64, f64) {
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
        assert!(super::quantile(f64::NAN, vec![1., 2.]).is_nan());
        // A rank on a value next to an infinity reads that value, not
        // Inf * 0 = NaN; a single +Inf is +Inf at any q.
        assert_eq!(super::quantile(0.5, vec![1., 2., f64::INFINITY]), 2.);
        assert_eq!(super::quantile(0.9, vec![f64::INFINITY]), f64::INFINITY);
    }

    // SQL `percentile_cont`: NaN after every number, interpolation between
    // closest ranks, no value for an empty group.
    #[test]
    fn percentile_cont_orders_nan_last() {
        assert_eq!(super::percentile_cont(0.5, vec![]), None);
        assert_eq!(super::percentile_cont(0.5, vec![3., 1., 2.]), Some(2.));
        assert_eq!(super::percentile_cont(0.25, vec![4., 1.]), Some(1.75));
        // [1, 2, NaN]: rank 0.5 is between 1 and 2; the largest is NaN.
        assert_eq!(
            super::percentile_cont(0.25, vec![f64::NAN, 2., 1.]),
            Some(1.5)
        );
        assert!(super::percentile_cont(1., vec![f64::NAN, 2., 1.])
            .unwrap()
            .is_nan());
        assert_eq!(super::quantile(0.5, vec![f64::NAN, 2., 1.]), 1.);
    }
}
