//! Windowed computations use Planner intents; deployments supply the input window.
use super::{
    values::{group_key, Value},
    Error,
};
use planner_types::pre_asap::{AggIntent, ColumnRef};
use std::collections::BTreeMap;

pub(super) fn reduce(
    rows: Vec<Vec<Value>>,
    intent: &AggIntent<ColumnRef>,
    groups: &[usize],
    coordinate: usize,
    value: usize,
    window: Option<(i64, i64)>,
) -> Result<Vec<Vec<Value>>, Error> {
    let mut grouped =
        BTreeMap::<Vec<Vec<u8>>, (Vec<Value>, Vec<(f64, f64)>, Vec<(i64, f64)>)>::new();
    for row in rows {
        let key = group_key(&row, groups)?;
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
    for (_, (mut keys, buckets, mut points)) in grouped {
        let result = if let AggIntent::HistogramQuantile { q } = intent {
            Some(Value::Float64(bucket_quantile(*q, buckets)))
        } else {
            points.sort_by_key(|p| p.0);
            let (start, end) =
                window.ok_or_else(|| Error::Invalid("missing temporal window".into()))?;
            if points.iter().any(|p| p.0 < start || p.0 > end)
                || points.windows(2).any(|p| p[0].0 == p[1].0)
            {
                return Err(Error::Invalid(
                    "duplicate or out-of-window timestamp".into(),
                ));
            }
            match intent {
                AggIntent::Rate => rate(&points, start, end).map(Value::Float64),
                AggIntent::Increase => rate(&points, start, end)
                    .map(|v| Value::Float64(v * (end as f64 - start as f64) / 1000.)),
                AggIntent::Count { .. } => Some(Value::Int64(
                    i64::try_from(points.len())
                        .map_err(|_| Error::Invalid("count overflow".into()))?,
                )),
                AggIntent::Sum { .. } => Some(Value::Float64(points.iter().map(|p| p.1).sum())),
                AggIntent::Avg { .. } => Some(Value::Float64(
                    points.iter().map(|p| p.1).sum::<f64>() / points.len() as f64,
                )),
                AggIntent::Min { .. } => {
                    Some(Value::Float64(points.iter().fold(f64::NAN, |a, p| {
                        if a.is_nan() || p.1 < a {
                            p.1
                        } else {
                            a
                        }
                    })))
                }
                AggIntent::Max { .. } => {
                    Some(Value::Float64(points.iter().fold(f64::NAN, |a, p| {
                        if a.is_nan() || p.1 > a {
                            p.1
                        } else {
                            a
                        }
                    })))
                }
                _ => return Err(Error::Invalid("unsupported temporal intent".into())),
            }
        };
        if let Some(result) = result {
            keys.push(result);
            output.push(keys);
        }
    }
    Ok(output)
}

fn rate(points: &[(i64, f64)], start: i64, end: i64) -> Option<f64> {
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
        if pair[1].1 < pair[0].1 {
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
    if delta > 0. && first >= 0. {
        to_start = to_start.min(span * first / delta);
    }
    if to_end >= average * 1.1 {
        to_end = average / 2.;
    }
    Some(delta * (span + to_start + to_end) / span / ((end as f64 - start as f64) / 1000.))
}

fn bucket_quantile(q: f64, mut b: Vec<(f64, f64)>) -> f64 {
    if q.is_nan() {
        return f64::NAN;
    }
    if q < 0. {
        return f64::NEG_INFINITY;
    }
    if q > 1. {
        return f64::INFINITY;
    }
    b.retain(|p| !p.0.is_nan());
    b.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut buckets: Vec<(f64, f64)> = Vec::new();
    for p in b {
        if let Some(last) = buckets.last_mut() {
            if last.0 == p.0 {
                last.1 += p.1;
                continue;
            }
        }
        buckets.push(p);
    }
    if buckets.len() < 2 || buckets.last().unwrap().0 != f64::INFINITY {
        return f64::NAN;
    }
    let mut prev = buckets[0].1;
    for p in buckets.iter_mut().skip(1) {
        if p.1 < prev || (p.1 - prev).abs() <= 1e-12 * (p.1.abs() + prev.abs()) {
            p.1 = prev;
        }
        prev = p.1;
    }
    let count = buckets.last().unwrap().1;
    if count == 0. {
        return f64::NAN;
    }
    let rank = q * count;
    let idx = buckets[..buckets.len() - 1].partition_point(|p| p.1 < rank);
    if idx == buckets.len() - 1 {
        return buckets[idx - 1].0;
    }
    if idx == 0 && buckets[0].0 <= 0. {
        return buckets[0].0;
    }
    let (start, base) = if idx == 0 { (0., 0.) } else { buckets[idx - 1] };
    let (end, upper) = buckets[idx];
    start + (end - start) * (rank - base) / (upper - base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::{
        batch_execution::evaluate_batch, operators::Operator, values::Batch, Limits, RunContext,
        Scope,
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
        assert_eq!(
            bucket_quantile(0.5, vec![(1., 1.), (1., 1.), (2., 4.), (f64::INFINITY, 4.)]),
            1.
        );
        assert!(bucket_quantile(0.5, vec![(1., 2.), (2., 4.)]).is_nan());
        assert_eq!(bucket_quantile(-0.1, vec![]), f64::NEG_INFINITY);
    }
}
