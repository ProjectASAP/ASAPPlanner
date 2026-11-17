//! PromQL range functions over each series' samples in a time window
//! (`rate`, `irate`, `changes`, `mad_over_time`, …), and histogram bucket
//! quantiles; the deployment supplies the window.
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
pub(in crate::operators) async fn reduce_batch(
    batch: &crate::values::Batch,
    intent: &AggIntent,
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
            // Rows usually arrive in time order; sort only when not.
            let points = if points.is_sorted_by_key(|p| p.0) {
                points
            } else {
                cooperative_sort(points, |a, b| a.0.cmp(&b.0), context).await?
            };
            let (start, end) =
                window.ok_or_else(|| Error::Invalid("missing temporal window".into()))?;
            if points.iter().any(|p| p.0 < start || p.0 > end)
                || points.windows(2).any(|p| p[0].0 == p[1].0)
            {
                return Err(Error::Invalid(
                    "duplicate or out-of-window timestamp".into(),
                ));
            }
            range_function_value(
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

/// Whether [`range_function_value`] reduces a series' samples for `intent`.
pub(crate) fn has_range_function_kernel(intent: &AggIntent) -> bool {
    matches!(
        intent,
        AggIntent::Rate
            | AggIntent::Deriv
            | AggIntent::PredictLinear { .. }
            | AggIntent::Increase
            | AggIntent::Delta
            | AggIntent::Count
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
pub(in crate::operators) fn range_function_value(
    intent: &AggIntent,
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
        AggIntent::Rate => extrapolated(points, start, end, true, true).map(Value::Float64),
        AggIntent::Delta => extrapolated(points, start, end, false, false).map(Value::Float64),
        AggIntent::Increase => extrapolated(points, start, end, true, false).map(Value::Float64),
        AggIntent::Count => Some(Value::Int64(
            i64::try_from(points.len()).map_err(|_| Error::Invalid("count overflow".into()))?,
        )),
        AggIntent::Sum { .. } | AggIntent::Avg { .. } => {
            let values = points.iter().map(|p| p.1).collect::<Vec<_>>();
            Some(Value::Float64(if matches!(intent, AggIntent::Sum { .. }) {
                super::aggregate::promql_sum(0., &values)
            } else {
                super::aggregate::promql_avg(&values)
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
        AggIntent::IRate => match points {
            [.., previous, last] => {
                crate::summary_kernels::increase::instant_rate(*previous, *last).map(Value::Float64)
            }
            _ => None,
        },
        // As Prometheus `instantValue`: no value for two samples at one time.
        AggIntent::IDelta => match points {
            [.., (t0, v0), (t1, v1)] if t0 != t1 => Some(Value::Float64(v1 - v0)),
            _ => None,
        },
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
        AggIntent::TsOfFirstOverTime => points.first().map(|p| sample_time_seconds(p.0)),
        AggIntent::TsOfLastOverTime | AggIntent::TimeFn(TimeFunc::Timestamp) => {
            points.last().map(|p| sample_time_seconds(p.0))
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
                .map(|p| sample_time_seconds(p.0))
        }
        // A NaN sample makes the median, and the deviation, undefined.
        AggIntent::MadOverTime if points.iter().any(|p| p.1.is_nan()) => {
            Some(Value::Float64(f64::NAN))
        }
        AggIntent::MadOverTime => (!points.is_empty()).then(|| {
            let median = super::aggregate::quantile(0.5, points.iter().map(|p| p.1).collect());
            Value::Float64(super::aggregate::quantile(
                0.5,
                points.iter().map(|p| (p.1 - median).abs()).collect(),
            ))
        }),
        AggIntent::DoubleExpSmoothing { smoothing, trend } => {
            double_exponential_smoothing(points, *smoothing, *trend)?.map(Value::Float64)
        }
        AggIntent::PresentOverTime => (!points.is_empty()).then_some(Value::Float64(1.)),
        AggIntent::Quantile { col: None, q, .. } => Some(Value::Float64(
            super::aggregate::quantile(*q, points.iter().map(|p| p.1).collect()),
        )),
        _ => return Err(Error::Invalid("unsupported temporal intent".into())),
    })
}

/// A sample time in milliseconds as PromQL's float seconds.
fn sample_time_seconds(timestamp_ms: i64) -> Value {
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

/// Prometheus `extrapolatedRate` over a series' sorted samples in `(start,
/// end]`; `counter` adds each reset's prior value and bounds the start by
/// the zero point, `is_rate` reads per second.
fn extrapolated(
    points: &[(i64, f64)],
    start: i64,
    end: i64,
    counter: bool,
    is_rate: bool,
) -> Option<f64> {
    let (&(first_ms, first), &(last_ms, last)) = (points.first()?, points.last()?);
    // Each reset adds the value before it, in sample order as Prometheus sums.
    let mut delta = last - first;
    if counter {
        for pair in points.windows(2) {
            if pair[1].1 < pair[0].1 {
                delta += pair[0].1;
            }
        }
    }
    crate::summary_kernels::increase::extrapolated_rate(
        crate::summary_kernels::increase::SampledDelta {
            first_sample: (first_ms, first),
            last_sample_ms: last_ms,
            sample_count: points.len() as u64,
            delta,
        },
        (start, end),
        counter,
        is_rate,
    )
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
            *acc = super::aggregate::kahan_inc(value, acc.0, acc.1);
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
    use planner_types::plan::TimeWindow;
    use planner_types::stages::Lifetime;

    use planner_types::ir::schema::{
        DataType, Field as SummaryField, FieldDataType as SummaryFamilyType, Schema,
    };
    use std::sync::Arc;

    // The same window operator must give the same answer in either engine
    // phase, and whatever order its rows arrive in.
    #[test]
    fn temporal_windows_execute_in_both_phases_and_count_is_integer() {
        let schema = Arc::new(Schema {
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
                (AggIntent::Count, Value::Int64(3)),
            ] {
                for time_ordered in [true, false] {
                    let mut rows = vec![
                        vec![Value::Timestamp(0), Value::Float64(2.)],
                        vec![Value::Timestamp(1000), Value::Float64(4.)],
                        vec![Value::Timestamp(2000), Value::Float64(2.)],
                    ];
                    if !time_ordered {
                        rows.reverse();
                    }
                    let batch = Batch::try_new(schema.clone(), rows).unwrap();
                    let operator = Operator::window(
                        schema.clone(),
                        intent.clone(),
                        0,
                        1,
                        vec![],
                        Some(TimeWindow {
                            ends_before_evaluation_ms: 0,
                            length_ms: 2000,
                        }),
                    )
                    .unwrap();
                    let mut dag = PhysicalDAG::default();
                    dag.add(
                        0,
                        vec![],
                        Operator::source(schema.clone(), vec![batch]).unwrap(),
                        Lifetime::None,
                    )
                    .unwrap();
                    dag.add(1, vec![0], operator, Lifetime::None).unwrap();
                    let context = RunContext::new(scope.clone(), Limits::default()).unwrap();
                    let result = if matches!(scope, Scope::Ingestion { .. }) {
                        let run = dag.execute_ingestion(&[1], context).unwrap();
                        futures::executor::block_on(run)
                            .unwrap()
                            .remove(0)
                            .remove(0)
                    } else {
                        let mut output = dag.execute(&[1], context).unwrap().remove(0);
                        futures::executor::block_on(output.next()).unwrap().unwrap()
                    };
                    assert_eq!(
                        format!("{:?}", result.to_rows()[0][0]),
                        format!("{expected:?}")
                    );
                }
            }
        }
        let empty = TimeWindow {
            ends_before_evaluation_ms: 0,
            length_ms: 0,
        };
        assert!(Operator::window(schema, AggIntent::Rate, 0, 1, vec![], Some(empty)).is_err());
    }

    // `rate` and `increase` extrapolate as Prometheus `extrapolatedRate`
    // (promql/functions.go, v3.16.0) in both kernels: a raw-sample window
    // and an exact counter state. Samples 1, 2, …, 17 every 15 s from 60 s,
    // read by `increase(x[5m])` at 300 s: the first sample lies 60 s after
    // the window start, beyond 1.1 average intervals, so the start
    // extrapolates half an interval (7.5 s); the zero point (15 s back) is
    // farther, so it does not shorten that. Prometheus 3.16.0 returns 16.5
    // and 0.055; bounding by the zero point before the half-interval cap
    // returned 17.
    #[test]
    fn rate_and_increase_extrapolate_as_prometheus_in_both_kernels() {
        use crate::evaluation::exact_evaluation;
        use crate::summary_kernels::exact::ExactAccumulator;
        use crate::AggregateCore;
        use planner_types::summary_type::{ExactKind, ExactParams};
        let evenly_spaced = (0..17)
            .map(|i| (60_000 + 15_000 * i, (i + 1) as f64))
            .collect::<Vec<_>>();
        // Irregular samples 2, 5, 6, 8 at 180 s, 240 s, 270 s, 300 s: the
        // start caps to 20 s, under the 40 s back to zero, so increase is
        // 6 × 140 / 120 = 7 (Prometheus 3.16.0); zero point first gave 8.
        let irregular = vec![(180_000, 2.), (240_000, 5.), (270_000, 6.), (300_000, 8.)];
        let (start, end) = (0, 300_000);
        for (points, intent, kind, params, expected) in [
            (
                &evenly_spaced,
                AggIntent::Increase,
                ExactKind::Increase,
                ExactParams::Increase,
                16.5,
            ),
            (
                &evenly_spaced,
                AggIntent::Rate,
                ExactKind::Rate,
                ExactParams::Rate,
                0.055,
            ),
            (
                &irregular,
                AggIntent::Increase,
                ExactKind::Increase,
                ExactParams::Increase,
                7.,
            ),
            (
                &irregular,
                AggIntent::Rate,
                ExactKind::Rate,
                ExactParams::Rate,
                7. / 300.,
            ),
        ] {
            let raw = match range_function_value(&intent, points, start, end, end).unwrap() {
                Some(Value::Float64(value)) => value,
                other => panic!("{other:?}"),
            };
            let mut state =
                ExactAccumulator::new(SummaryFamilyType::ExactAggregate(kind, params), false)
                    .unwrap();
            for &(time, value) in points {
                state.update(None, value, time);
            }
            let exact = exact_evaluation(
                [Arc::new(state) as Arc<dyn AggregateCore>],
                kind,
                Some((start, end)),
                None,
            )
            .unwrap()
            .unwrap();
            for value in [raw, exact] {
                assert!(
                    (value - expected).abs() <= 1e-12 * expected,
                    "{intent:?}: {value} != {expected}"
                );
            }
        }
    }

    // Each range function gives the value of its PromQL definition. Samples
    // 1, 4, 2, 4 at 10 s to 40 s; their two maxima tie.
    #[test]
    fn range_functions_match_prometheus() {
        let points = [(10_000, 1.), (20_000, 4.), (30_000, 2.), (40_000, 4.)];
        let value = |intent: AggIntent, points: &[(i64, f64)]| match range_function_value(
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
        // Two samples at one time: no instant rate or delta.
        let same_time = [(20_000, 5.), (20_000, 8.)];
        assert_eq!(value(AggIntent::IRate, &same_time), None);
        assert_eq!(value(AggIntent::IDelta, &same_time), None);
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
            assert!(range_function_value(&intent, &points, 0, 40_000, 40_000).is_err());
        }
        // A NaN extreme gives way to the next number; a later NaN does not
        // replace it.
        let nan = [(1_000, f64::NAN), (2_000, 3.), (3_000, f64::NAN)];
        assert_eq!(value(AggIntent::TsOfMaxOverTime, &nan), Some(2.));
        assert_eq!(value(AggIntent::TsOfMinOverTime, &nan), Some(2.));
        assert!(value(AggIntent::MadOverTime, &nan).unwrap().is_nan());
        // Infinite samples: median 3 of -Inf, 1, 3, 5, +Inf; deviations 0, 2,
        // 2, +Inf, +Inf have median 2 (Prometheus 3.16.0).
        let infinite = [
            (1_000, 1.),
            (2_000, f64::INFINITY),
            (3_000, 3.),
            (4_000, f64::NEG_INFINITY),
            (5_000, 5.),
        ];
        assert_eq!(value(AggIntent::MadOverTime, &infinite), Some(2.));
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
