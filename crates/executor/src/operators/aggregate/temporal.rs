//! Windowed computations use Planner intents; deployments supply the input window.
use crate::{
    operators::{common::Workspace, sort::cooperative_sort},
    runtime::{Cooperative, RunContext},
};
use crate::{values::Value, Error};
use planner_types::ir::operator::{AggIntent, TimeFunc};

/// Each group's temporal value over `batch`: the groups of `groups` in key
/// order, labelled as their first rows, each with its `intent` over its
/// samples (`coordinate` a Timestamp, or a Float64 bucket bound for a
/// histogram quantile; `value` Float64). A group whose value PromQL omits
/// has no row. The result is the label columns and the value column of
/// `output_value` type.
pub(in crate::operators) async fn reduce_batch<C>(
    batch: &crate::values::Batch,
    intent: &AggIntent<C>,
    groups: &[usize],
    coordinate: usize,
    value: usize,
    window: Option<(i64, i64)>,
    context: &RunContext,
) -> Result<(Vec<arrow::array::ArrayRef>, Vec<Value>), Error> {
    use arrow::array::{Array, Float64Array, TimestampMillisecondArray};
    let mut work = Cooperative::new(context);
    let mut workspace = Workspace::new(context)?;
    let mut table = crate::operators::group_table::GroupTable::new(groups.to_vec());
    let ids = crate::operators::common::chunked_group_ids(
        &mut table,
        batch,
        &mut workspace,
        16 * batch.num_rows(),
        &mut work,
    )
    .await?;
    let values = batch
        .column(value)
        .as_any()
        .downcast_ref::<Float64Array>()
        .ok_or_else(|| Error::Invalid("window value must be Float64".into()))?;
    let coordinates = batch.column(coordinate);
    let times = coordinates
        .as_any()
        .downcast_ref::<TimestampMillisecondArray>();
    let bounds = coordinates.as_any().downcast_ref::<Float64Array>();
    let (mut buckets, mut points) = (vec![Vec::new(); table.len()], vec![Vec::new(); table.len()]);
    for (row, &group) in ids.iter().enumerate() {
        if values.is_null(row) {
            return Err(Error::Invalid("window value must be Float64".into()));
        }
        let v = values.value(row);
        match (times, bounds) {
            (Some(times), _) if times.is_valid(row) => {
                points[group as usize].push((times.value(row), v))
            }
            (_, Some(bounds)) if bounds.is_valid(row) => {
                buckets[group as usize].push((bounds.value(row), v))
            }
            _ => return Err(Error::Invalid("invalid window coordinate".into())),
        }
    }
    work.checkpoint_rows(batch.num_rows()).await?;
    workspace.set_bytes(table.memory_bytes() + 16 * batch.num_rows())?;
    let mut kept = Vec::new();
    let mut results = Vec::new();
    for group in table.groups_in_key_order() {
        work.checkpoint().await?;
        let buckets = std::mem::take(&mut buckets[group as usize]);
        let points = std::mem::take(&mut points[group as usize]);
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
            kept.push(group);
            results.push(result);
        }
    }
    let labels = table.label_arrays(batch.schema(), &kept)?;
    Ok((labels, results))
}

/// Whether [`window_value`] reduces a series' samples for `intent`.
pub(in crate::operators) fn has_window_kernel<C>(intent: &AggIntent<C>) -> bool {
    matches!(
        intent,
        AggIntent::Rate
            | AggIntent::Deriv
            | AggIntent::PredictLinear { .. }
            | AggIntent::Increase
            | AggIntent::Delta
            | AggIntent::Count { .. }
            | AggIntent::Sum { col: None }
            | AggIntent::Avg { col: None }
            | AggIntent::Min { col: None }
            | AggIntent::Max { col: None }
            | AggIntent::IRate
            | AggIntent::IDelta
            | AggIntent::Changes
            | AggIntent::Resets
            | AggIntent::LastOverTime
            | AggIntent::FirstOverTime
            | AggIntent::TsOfFirstOverTime
            | AggIntent::TsOfLastOverTime
            | AggIntent::TsOfMinOverTime
            | AggIntent::TsOfMaxOverTime
            | AggIntent::MadOverTime
            | AggIntent::DoubleExpSmoothing { .. }
            | AggIntent::PresentOverTime
            | AggIntent::TimeFn(TimeFunc::Timestamp)
            | AggIntent::Quantile { col: None, .. }
    )
}

/// One series' value over its sorted samples in the window `(start, end]`.
/// `None` means PromQL emits no sample for this series.
pub(in crate::operators) fn window_value<C>(
    intent: &AggIntent<C>,
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
        AggIntent::FirstOverTime => points.first().map(|p| Value::Float64(p.1)),
        AggIntent::TsOfFirstOverTime => points.first().map(|p| seconds(p.0)),
        AggIntent::TsOfLastOverTime | AggIntent::TimeFn(TimeFunc::Timestamp) => {
            points.last().map(|p| seconds(p.0))
        }
        // Prometheus keeps the latest of tied extremes, and a NaN only
        // until a number follows.
        AggIntent::TsOfMinOverTime | AggIntent::TsOfMaxOverTime => {
            let min = matches!(intent, AggIntent::TsOfMinOverTime);
            points
                .iter()
                .copied()
                .reduce(|best, p| {
                    let better = if min { p.1 <= best.1 } else { p.1 >= best.1 };
                    if better || best.1.is_nan() {
                        p
                    } else {
                        best
                    }
                })
                .map(|p| seconds(p.0))
        }
        // A NaN sample makes the median, and the deviation, undefined.
        AggIntent::MadOverTime if points.iter().any(|p| p.1.is_nan()) => {
            Some(Value::Float64(f64::NAN))
        }
        AggIntent::MadOverTime => (!points.is_empty()).then(|| {
            let median = super::quantile(0.5, points.iter().map(|p| p.1).collect());
            Value::Float64(super::quantile(
                0.5,
                points.iter().map(|p| (p.1 - median).abs()).collect(),
            ))
        }),
        AggIntent::DoubleExpSmoothing { smoothing, trend } => {
            double_exponential_smoothing(points, *smoothing, *trend)?.map(Value::Float64)
        }
        AggIntent::PresentOverTime => (!points.is_empty()).then_some(Value::Float64(1.)),
        AggIntent::Quantile { col: None, q, .. } => Some(Value::Float64(super::quantile(
            *q,
            points.iter().map(|p| p.1).collect(),
        ))),
        _ => return Err(Error::Invalid("unsupported temporal intent".into())),
    })
}

/// A sample time in milliseconds as PromQL's float seconds.
fn seconds(timestamp_ms: i64) -> Value {
    Value::Float64(timestamp_ms as f64 / 1000.)
}

/// Prometheus `double_exponential_smoothing`: no value under two samples,
/// and an error unless both factors lie in (0, 1).
fn double_exponential_smoothing(
    points: &[(i64, f64)],
    smoothing: f64,
    trend: f64,
) -> Result<Option<f64>, Error> {
    for (name, factor) in [("smoothing", smoothing), ("trend", trend)] {
        if !(factor > 0. && factor < 1.) {
            return Err(Error::Invalid(format!(
                "invalid {name} factor: expected 0 < {name} < 1, got {factor}"
            )));
        }
    }
    let [(_, first), (_, second), ..] = points else {
        return Ok(None);
    };
    let (mut previous, mut level, mut slope) = (0., *first, second - first);
    for (i, &(_, value)) in points.iter().enumerate().skip(1) {
        if i > 1 {
            slope = trend * (level - previous) + (1. - trend) * slope;
        }
        (previous, level) = (
            level,
            smoothing * value + (1. - smoothing) * (level + slope),
        );
    }
    Ok(Some(level))
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
        plan::PhysicalDAG,
        runtime::{Limits, RunContext, Scope},
        values::Batch,
    };
    use futures::StreamExt;
    use planner_types::ir::properties::AccuracyTarget;
    use planner_types::ir::schema::{
        DataType, Field as SummaryField, FieldDataType as SummaryFamilyType, Schema,
    };
    use std::sync::Arc;

    // The same window operator must give the same answer in either engine phase.
    #[test]
    fn temporal_windows_execute_in_both_phases_and_count_is_integer() {
        let schema = Arc::new(Schema {
            fields: vec![
                SummaryField {
                    name: "time".into(),
                    dtype: SummaryFamilyType::Plain(DataType::Timestamp),
                    nullable: false,
                    table: None,
                },
                SummaryField {
                    name: "value".into(),
                    dtype: SummaryFamilyType::Plain(DataType::Float64),
                    nullable: false,
                    table: None,
                },
            ],
            time_index: Some(0),
            unique_keys: vec![],
            closed: false,
        });
        for scope in [
            Scope::Query {
                evaluation_time_ms: 2000,
            },
            Scope::Ingestion {
                window_start_ms: 0,
                window_end_ms: 2000,
            },
        ] {
            for (intent, expected) in [
                (AggIntent::Rate, Value::Float64(2.)),
                (AggIntent::Increase, Value::Float64(4.)),
                (
                    AggIntent::Count {
                        accuracy: AccuracyTarget::exact(),
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
                let mut dag = PhysicalDAG::default();
                dag.add(
                    0,
                    vec![],
                    Operator::source(schema.clone(), vec![batch]).unwrap(),
                )
                .unwrap();
                dag.add(1, vec![0], operator).unwrap();
                let context = RunContext::new(scope.clone(), Limits::default()).unwrap();
                let mut output = dag.execute(&[1], context).unwrap().remove(0);
                let result = futures::executor::block_on(output.next()).unwrap().unwrap();
                assert_eq!(
                    format!("{:?}", result.to_rows()[0][0]),
                    format!("{expected:?}")
                );
            }
        }
        assert!(Operator::window(schema, AggIntent::Rate, 0, 1, vec![], Some((0, 0))).is_err());
    }

    // Each range function gives the value of its PromQL definition. Samples
    // 1, 4, 2, 4 at 10 s to 40 s; their two maxima tie.
    #[test]
    fn range_functions_match_prometheus() {
        let points = [(10_000, 1.), (20_000, 4.), (30_000, 2.), (40_000, 4.)];
        let value = |intent: AggIntent, points: &[(i64, f64)]| match window_value(
            &intent, points, 0, 40_000, 40_000,
        )
        .unwrap()
        {
            Some(Value::Float64(v)) => Some(v),
            None => None,
            other => panic!("{other:?}"),
        };
        for (intent, expected) in [
            (AggIntent::FirstOverTime, 1.),
            (AggIntent::LastOverTime, 4.),
            (AggIntent::TsOfFirstOverTime, 10.),
            (AggIntent::TsOfLastOverTime, 40.),
            (AggIntent::TimeFn(TimeFunc::Timestamp), 40.),
            (AggIntent::TsOfMinOverTime, 10.),
            // The later of the tied maxima.
            (AggIntent::TsOfMaxOverTime, 40.),
            // Median 3; deviations 2, 1, 1, 1 have median 1.
            (AggIntent::MadOverTime, 1.),
            (AggIntent::PresentOverTime, 1.),
            // Value changes 1→4, 4→2, 2→4; one decrease 4→2.
            (AggIntent::Changes, 3.),
            (AggIntent::Resets, 1.),
            // Levels 1, 4, 4.5, 5.125 with trends 3, 3, 1.75 (by hand from
            // Prometheus `funcDoubleExponentialSmoothing`).
            (
                AggIntent::DoubleExpSmoothing {
                    smoothing: 0.5,
                    trend: 0.5,
                },
                5.125,
            ),
            // The last two samples 2, 4 over 10 s.
            (AggIntent::IDelta, 2.),
            (AggIntent::IRate, 0.2),
        ] {
            assert_eq!(value(intent.clone(), &points), Some(expected), "{intent:?}");
        }
        // A reset between the last two samples: the newer value is the increase.
        assert_eq!(
            value(AggIntent::IRate, &[(10_000, 8.), (20_000, 3.)]),
            Some(0.3)
        );
        // One sample: no instant rate, no smoothing; one value has no change.
        let single = [(10_000, 5.)];
        assert_eq!(value(AggIntent::IRate, &single), None);
        assert_eq!(value(AggIntent::IDelta, &single), None);
        assert_eq!(value(AggIntent::Changes, &single), Some(0.));
        assert_eq!(value(AggIntent::MadOverTime, &single), Some(0.));
        let smoothing = AggIntent::DoubleExpSmoothing {
            smoothing: 0.5,
            trend: 0.5,
        };
        assert_eq!(value(smoothing, &single), None);
        // A factor outside (0, 1) is an error, as Prometheus panics on it.
        for (smoothing, trend) in [(1., 0.5), (0.5, 0.), (f64::NAN, 0.5)] {
            let intent: AggIntent = AggIntent::DoubleExpSmoothing { smoothing, trend };
            assert!(window_value(&intent, &points, 0, 40_000, 40_000).is_err());
        }
        // A NaN extreme gives way to the next number; a later NaN does not
        // replace it.
        let nan = [(1_000, f64::NAN), (2_000, 3.), (3_000, f64::NAN)];
        assert_eq!(value(AggIntent::TsOfMaxOverTime, &nan), Some(2.));
        assert_eq!(value(AggIntent::TsOfMinOverTime, &nan), Some(2.));
        assert!(value(AggIntent::MadOverTime, &nan).unwrap().is_nan());
    }

    // Histogram interpolation requires an infinite terminal bucket and coalesces duplicates.
    #[test]
    fn histogram_boundaries_and_duplicate_buckets() {
        let context = RunContext::new(
            Scope::Query {
                evaluation_time_ms: 0,
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
