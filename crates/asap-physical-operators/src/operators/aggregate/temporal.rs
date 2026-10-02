//! Windowed computations use Planner intents; deployments supply the input window.
use crate::{
    operators::{
        common::{key_bytes, row_bytes, Workspace},
        sort::cooperative_sort,
    },
    runtime::{Cooperative, RunContext},
};
use crate::{
    values::{group_key, Value},
    Error,
};
use planner_types::pre_asap::{AggIntent, ColumnRef};
use std::collections::BTreeMap;

pub(in crate::operators) async fn reduce(
    rows: Vec<Vec<Value>>,
    intent: &AggIntent<ColumnRef>,
    groups: &[usize],
    coordinate: usize,
    value: usize,
    window: Option<(i64, i64)>,
    context: &RunContext,
) -> Result<Vec<Vec<Value>>, Error> {
    let mut work = Cooperative::new(context);
    let mut workspace = Workspace::new(context)?;
    let mut grouped =
        BTreeMap::<Vec<Vec<u8>>, (Vec<Value>, Vec<(f64, f64)>, Vec<(i64, f64)>)>::new();
    for row in rows {
        work.checkpoint().await?;
        let key = group_key(&row, groups)?;
        workspace.grow(32)?;
        if !grouped.contains_key(&key) {
            workspace.grow(key_bytes(&key) + row_bytes(&row))?;
        }
        let entry = grouped.entry(key).or_insert_with(|| {
            (
                groups.iter().map(|i| row[*i].clone()).collect(),
                vec![],
                vec![],
            )
        });
        let Value::Float64(v) = row[value] else {
            return Err(Error::Invalid("window value must be Float64".into()));
        };
        match row[coordinate] {
            Value::Timestamp(t) => entry.2.push((t, v)),
            Value::Float64(bound) => entry.1.push((bound, v)),
            _ => return Err(Error::Invalid("invalid window coordinate".into())),
        }
    }
    let mut output = Vec::new();
    for (_, (mut keys, buckets, points)) in grouped {
        work.checkpoint().await?;
        let result = if let AggIntent::HistogramQuantile { q, .. } = intent {
            Some(Value::Float64(bucket_quantile(*q, buckets, context).await?))
        } else {
            let points = cooperative_sort(points, |a, b| a.0.cmp(&b.0), context).await?;
            let (start, end) =
                window.ok_or_else(|| Error::Invalid("missing temporal window".into()))?;
            if points.iter().any(|p| p.0 < start || p.0 > end)
                || points.windows(2).any(|p| p[0].0 == p[1].0)
            {
                return Err(Error::Invalid(
                    "duplicate or out-of-window timestamp".into(),
                ));
            }
            window_value(
                intent,
                &points,
                start,
                end,
                match context.scope {
                    crate::runtime::Scope::Query {
                        evaluation_time_ms, ..
                    } => evaluation_time_ms,
                    _ => end,
                },
            )?
        };
        if let Some(result) = result {
            keys.push(result);
            output.push(keys);
        }
    }
    Ok(output)
}

/// One series' value over its sorted samples in the window `(start, end]`.
/// `None` means PromQL emits no sample for this series.
pub(in crate::operators) fn window_value(
    intent: &AggIntent<ColumnRef>,
    points: &[(i64, f64)],
    start: i64,
    end: i64,
    evaluation_time: i64,
) -> Result<Option<Value>, Error> {
    Ok(match intent {
        AggIntent::Deriv => regression(points, points.first().map_or(0, |p| p.0))
            .map(|(slope, _)| Value::Float64(slope)),
        AggIntent::PredictLinear { seconds } => regression(points, evaluation_time)
            .map(|(slope, intercept)| Value::Float64(slope * seconds + intercept)),
        AggIntent::Rate => rate(points, start, end, true).map(Value::Float64),
        AggIntent::Delta => rate(points, start, end, false)
            .map(|v| Value::Float64(v * (end as f64 - start as f64) / 1000.)),
        AggIntent::Increase => rate(points, start, end, true)
            .map(|v| Value::Float64(v * (end as f64 - start as f64) / 1000.)),
        AggIntent::Count { .. } => Some(Value::Int64(
            i64::try_from(points.len()).map_err(|_| Error::Invalid("count overflow".into()))?,
        )),
        AggIntent::Sum { .. } | AggIntent::Avg { .. } => {
            let values = points.iter().map(|p| p.1).collect::<Vec<_>>();
            Some(Value::Float64(if matches!(intent, AggIntent::Sum { .. }) {
                super::promql_sum(0., &values)
            } else {
                super::promql_avg(&values)
            }))
        }
        AggIntent::Min { .. } => Some(Value::Float64(points.iter().fold(f64::NAN, |a, p| {
            if a.is_nan() || p.1 < a {
                p.1
            } else {
                a
            }
        }))),
        AggIntent::Max { .. } => Some(Value::Float64(points.iter().fold(f64::NAN, |a, p| {
            if a.is_nan() || p.1 > a {
                p.1
            } else {
                a
            }
        }))),
        AggIntent::IRate | AggIntent::IDelta => {
            let [.., (t0, v0), (t1, v1)] = points else {
                return Ok(None);
            };
            let rate = matches!(intent, AggIntent::IRate);
            // A counter reset makes the last value the increase.
            let delta = if rate && v1 < v0 { *v1 } else { v1 - v0 };
            match (rate, t1 - t0) {
                (_, 0) => None,
                (true, interval) => Some(Value::Float64(delta / (interval as f64 / 1000.))),
                (false, _) => Some(Value::Float64(delta)),
            }
        }
        AggIntent::Changes | AggIntent::Resets => {
            let changed = |(a, b): (f64, f64)| match intent {
                AggIntent::Changes => a != b && !(a.is_nan() && b.is_nan()),
                _ => b < a,
            };
            let count = points
                .windows(2)
                .filter(|pair| changed((pair[0].1, pair[1].1)))
                .count();
            Some(Value::Float64(count as f64))
        }
        AggIntent::LastOverTime => points.last().map(|p| Value::Float64(p.1)),
        AggIntent::Quantile { col: None, q, .. } => Some(Value::Float64(super::quantile(
            *q,
            points.iter().map(|p| p.1).collect(),
        ))),
        _ => return Err(Error::Invalid("unsupported temporal intent".into())),
    })
}

/// Prometheus `extrapolatedRate`; `counter` enables reset correction and the zero bound.
fn rate(points: &[(i64, f64)], start: i64, end: i64, counter: bool) -> Option<f64> {
    if points.len() < 2 {
        return None;
    }
    let (first_t, first) = points[0];
    let (last_t, last) = *points.last()?;
    let span = (last_t as f64 - first_t as f64) / 1000.;
    if span <= 0. {
        return None;
    }
    let mut delta = last - first;
    for pair in points.windows(2) {
        if counter && pair[1].1 < pair[0].1 {
            delta += pair[0].1;
        }
    }
    let average = span / (points.len() - 1) as f64;
    let mut to_start = (first_t as f64 - start as f64) / 1000.;
    let mut to_end = (end as f64 - last_t as f64) / 1000.;
    if to_start >= average * 1.1 {
        to_start = average / 2.;
    }
    // Apply the zero bound after the sparse-window half-interval cap.
    if counter && delta > 0. && first >= 0. {
        to_start = to_start.min(span * first / delta);
    }
    if to_end >= average * 1.1 {
        to_end = average / 2.;
    }
    Some(delta * (span + to_start + to_end) / span / ((end as f64 - start as f64) / 1000.))
}

pub(in crate::operators) async fn bucket_quantile(
    q: f64,
    mut b: Vec<(f64, f64)>,
    context: &RunContext,
) -> Result<f64, Error> {
    let mut work = Cooperative::new(context);
    let _scratch = context.reserve(b.len().checked_mul(16).ok_or(Error::MemoryLimit)?)?;
    if q.is_nan() {
        return Ok(f64::NAN);
    }
    if q < 0. {
        return Ok(f64::NEG_INFINITY);
    }
    if q > 1. {
        return Ok(f64::INFINITY);
    }
    b.retain(|p| !p.0.is_nan());
    b = cooperative_sort(b, |a, b| a.0.total_cmp(&b.0), context).await?;
    let mut buckets: Vec<(f64, f64)> = Vec::new();
    for p in b {
        work.checkpoint().await?;
        if let Some(last) = buckets.last_mut() {
            if last.0 == p.0 {
                last.1 += p.1;
                continue;
            }
        }
        buckets.push(p);
    }
    if buckets.len() < 2 || buckets.last().unwrap().0 != f64::INFINITY {
        return Ok(f64::NAN);
    }
    let mut prev = buckets[0].1;
    for p in buckets.iter_mut().skip(1) {
        work.checkpoint().await?;
        if p.1 < prev || almost_equal(prev, p.1) {
            p.1 = prev;
        }
        prev = p.1;
    }
    let count = buckets.last().unwrap().1;
    if count == 0. {
        return Ok(f64::NAN);
    }
    let rank = q * count;
    let idx = buckets[..buckets.len() - 1].partition_point(|p| {
        // Go searches for `count >= rank`; a NaN comparison is never a match.
        !matches!(
            p.1.partial_cmp(&rank),
            Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
        )
    });
    if idx == buckets.len() - 1 {
        return Ok(buckets[idx - 1].0);
    }
    if idx == 0 && buckets[0].0 <= 0. {
        return Ok(buckets[0].0);
    }
    let (start, base) = if idx == 0 { (0., 0.) } else { buckets[idx - 1] };
    let (end, upper) = buckets[idx];
    Ok(start + (end - start) * ((rank - base) / (upper - base)))
}

/// Prometheus `almost.Equal` with its bucket tolerance of 1e-12.
fn almost_equal(a: f64, b: f64) -> bool {
    const EPSILON: f64 = 1e-12;
    if a == b || (a.is_nan() && b.is_nan()) {
        return true;
    }
    let sum = a.abs() + b.abs();
    let diff = (a - b).abs();
    if a == 0. || b == 0. || sum < f64::MIN_POSITIVE {
        return diff < EPSILON * f64::MIN_POSITIVE;
    }
    diff / sum.min(f64::MAX) < EPSILON
}

// Center timestamps before fitting and compensate sums to preserve small slopes.
fn regression(points: &[(i64, f64)], anchor: i64) -> Option<(f64, f64)> {
    if points.len() < 2 {
        return None;
    }
    let first = points[0].1;
    if points.iter().all(|p| p.1 == first) {
        return Some(if first.is_finite() {
            (0., first)
        } else {
            (f64::NAN, f64::NAN)
        });
    }
    let mut sums = [(0., 0.); 4];
    for &(timestamp, y) in points {
        let x = (i128::from(timestamp) - i128::from(anchor)) as f64 / 1000.;
        for (acc, value) in sums.iter_mut().zip([x, y, x * y, x * x]) {
            *acc = super::kahan_inc(value, acc.0, acc.1);
        }
    }
    let [sx, sy, sxy, sxx] = sums.map(|(s, c)| s + c);
    let n = points.len() as f64;
    let slope = (sxy - sx * sy / n) / (sxx - sx * sx / n);
    Some((slope, sy / n - slope * sx / n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        operators::Operator,
        runtime::{batch_execution::evaluate_batch, Limits, RunContext, Scope},
        values::Batch,
    };
    use planner_types::{
        post_asap::{SummaryFamilyType, SummaryField, SummarySchema},
        pre_asap::DataType,
        types::AccuracyTarget,
    };
    use std::sync::Arc;

    // The same window operator must give the same answer in either engine phase.
    #[test]
    fn temporal_windows_execute_in_both_phases_and_count_is_integer() {
        let schema = Arc::new(SummarySchema {
            fields: vec![
                SummaryField {
                    name: "time".into(),
                    dtype: SummaryFamilyType::Plain(DataType::Timestamp),
                    nullable: false,
                },
                SummaryField {
                    name: "value".into(),
                    dtype: SummaryFamilyType::Plain(DataType::Float64),
                    nullable: false,
                },
            ],
            time_index: Some(0),
        });
        for scope in [
            Scope::Query {
                evaluation_time_ms: 2000,
                revision: 1,
            },
            Scope::Ingestion {
                window_start_ms: 0,
                window_end_ms: 2000,
                revision: 1,
            },
        ] {
            for (intent, expected) in [
                (AggIntent::Rate, Value::Float64(2.)),
                (AggIntent::Increase, Value::Float64(4.)),
                (
                    AggIntent::Count {
                        accuracy: AccuracyTarget::Exact,
                    },
                    Value::Int64(3),
                ),
            ] {
                let batch = Batch::try_new(
                    schema.clone(),
                    vec![
                        vec![Value::Timestamp(0), Value::Float64(2.)],
                        vec![Value::Timestamp(1000), Value::Float64(4.)],
                        vec![Value::Timestamp(2000), Value::Float64(2.)],
                    ],
                )
                .unwrap();
                let operator =
                    Operator::window(schema.clone(), intent, 0, 1, vec![], Some((0, 2000)))
                        .unwrap();
                let result = evaluate_batch(
                    batch,
                    vec![operator],
                    RunContext::new(scope.clone(), Limits::default()).unwrap(),
                )
                .unwrap();
                assert_eq!(
                    format!("{:?}", result[0].rows()[0][0]),
                    format!("{expected:?}")
                );
            }
        }
        assert!(Operator::window(schema, AggIntent::Rate, 0, 1, vec![], Some((1, 1))).is_err());
    }

    // Histogram interpolation requires an infinite terminal bucket and coalesces duplicates.
    #[test]
    fn histogram_boundaries_and_duplicate_buckets() {
        let context = RunContext::new(
            Scope::Query {
                evaluation_time_ms: 0,
                revision: 0,
            },
            Limits::default(),
        )
        .unwrap();
        let bucket_quantile = |q, buckets| {
            futures::executor::block_on(super::bucket_quantile(q, buckets, &context)).unwrap()
        };
        assert_eq!(
            bucket_quantile(0.5, vec![(1., 1.), (1., 1.), (2., 4.), (f64::INFINITY, 4.)]),
            1.
        );
        assert!(bucket_quantile(0.5, vec![(1., 2.), (2., 4.)]).is_nan());
        assert_eq!(bucket_quantile(-0.1, vec![]), f64::NEG_INFINITY);
    }

    // Matches Prometheus bit for bit: interpolation divides before scaling,
    // and an infinite count is not "almost equal" to a finite one.
    #[test]
    fn histogram_matches_prometheus_arithmetic() {
        let context = RunContext::new(
            Scope::Query {
                evaluation_time_ms: 0,
                revision: 0,
            },
            Limits::default(),
        )
        .unwrap();
        let bucket_quantile = |q, buckets| {
            futures::executor::block_on(super::bucket_quantile(q, buckets, &context)).unwrap()
        };
        // 0.5 + (0.8 - 0.5) * ((19.95 - 10) / 11) in Go.
        assert_eq!(
            bucket_quantile(0.95, vec![(0.5, 10.), (0.8, 21.), (f64::INFINITY, 21.)]),
            0.7713636363636364
        );
        // Rank ∞ lies past every finite bucket.
        assert_eq!(
            bucket_quantile(
                0.5,
                vec![(1., 1.), (2., 2.), (f64::INFINITY, f64::INFINITY)]
            ),
            2.
        );
        // A NaN rank (0 · ∞, or a NaN total) finds no bucket, as Go's sort.Search.
        assert_eq!(
            bucket_quantile(0., vec![(1., 1.), (2., 2.), (f64::INFINITY, f64::INFINITY)]),
            2.
        );
        assert_eq!(
            bucket_quantile(0.5, vec![(1., 1.), (2., 2.), (f64::INFINITY, f64::NAN)]),
            2.
        );
    }
}
