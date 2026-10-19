//! The row-at-a-time `SeriesWindow` (reference for tests).
use super::super::series_window::{anchor, evaluation_time, evaluation_times, STALE_MARKER};
use super::super::*;

pub(super) fn execute<'a>(
    operator: &'a Operator,
    mut inputs: Vec<Input<'a, Batch>>,
    context: RunContext,
) -> Result<OutputStream<'a, Batch>, Error> {
    let Kind::SeriesWindow {
        function,
        coordinate,
        value,
        range_ms,
        offset_ms,
        at_ms,
        steps,
        range_at,
        steps_range_at,
    } = &operator.kind
    else {
        unreachable!()
    };
    let (coordinate, value) = (*coordinate, *value);
    let input = inputs
        .pop()
        .ok_or_else(|| invalid("series window input missing"))?;
    let mut resolved_steps = *steps;
    if let Some(steps) = &mut resolved_steps {
        steps.at_ms = anchor(&context, steps.at_ms, *steps_range_at)?;
    }
    let at_ms = anchor(&context, *at_ms, *range_at)?;
    let times = evaluation_times(evaluation_time(&context)?, resolved_steps)?;
    // A window pinned by `@` is step-invariant in Prometheus: it is evaluated
    // once, at the first instant of the query (or of its subquery grid), and
    // that result is reused at every step. Only predict_linear reads the time.
    let invariant_time = match (at_ms, context.query_range()) {
        (None, _) => None,
        (Some(_), None) => Some(times.0),
        (Some(_), Some((start, _))) => Some(evaluation_times(start, resolved_steps)?.0),
    };
    Ok(futures::stream::once(async move {
        let (rows, _memory) = collect_rows(input, &context).await?;
        let mut work = Cooperative::new(&context);
        let mut workspace = Workspace::new(&context)?;
        let identity = (0..operator.output.fields.len())
            .filter(|&i| i != coordinate && i != value)
            .collect::<Vec<_>>();
        let mut series = BTreeMap::<Vec<Vec<u8>>, (usize, Vec<(i64, f64)>)>::new();
        for (index, row) in rows.iter().enumerate() {
            work.checkpoint().await?;
            let key = group_key(row, &identity)?;
            let (Value::Timestamp(time), Value::Float64(sample)) = (&row[coordinate], &row[value])
            else {
                return Err(invalid("series window requires time and value samples"));
            };
            workspace.grow(16)?;
            if !series.contains_key(&key) {
                workspace.grow(key_bytes(&key) + 64)?;
            }
            series
                .entry(key)
                .or_insert_with(|| (index, Vec::new()))
                .1
                .push((*time, *sample));
        }
        for (_, points) in series.values_mut() {
            work.checkpoint().await?;
            points.sort_by_key(|p| p.0);
            if points.windows(2).any(|p| p[0].0 == p[1].0) {
                return Err(invalid("duplicate sample timestamp for one series"));
            }
        }
        let mut output = Vec::new();
        let (mut time, last, step) = times;
        while time <= last && !series.is_empty() {
            work.checkpoint().await?;
            let overflow = || invalid("series window overflows");
            let end = at_ms
                .unwrap_or(time)
                .checked_sub(*offset_ms)
                .ok_or_else(overflow)?;
            let start = end.checked_sub(*range_ms).ok_or_else(overflow)?;
            for (template, points) in series.values() {
                work.checkpoint().await?;
                // PromQL ranges are left-open: a sample at `start` is outside.
                let first = points.partition_point(|p| p.0 <= start);
                let last = points.partition_point(|p| p.0 <= end);
                let points = &points[first..last];
                let result = match function {
                    None => points
                        .last()
                        .filter(|p| p.1.to_bits() != STALE_MARKER)
                        .map(|p| p.1),
                    Some(intent) => {
                        let fresh = points
                            .iter()
                            .copied()
                            .filter(|p| p.1.to_bits() != STALE_MARKER)
                            .collect::<Vec<_>>();
                        if fresh.is_empty() {
                            None
                        } else {
                            match aggregate::temporal::window_value(
                                intent,
                                &fresh,
                                start,
                                end,
                                invariant_time.unwrap_or(time),
                            )? {
                                Some(Value::Float64(v)) => Some(v),
                                Some(Value::Int64(v)) => Some(v as f64),
                                Some(_) => return Err(invalid("invalid range function result")),
                                None => None,
                            }
                        }
                    }
                };
                if let Some(result) = result {
                    let mut row = rows[*template].clone();
                    row[coordinate] = Value::Timestamp(time);
                    row[value] = Value::Float64(result);
                    workspace.grow(row_bytes(&row))?;
                    output.push(row);
                }
            }
            let Some(next) = time.checked_add(step) else {
                break;
            };
            time = next;
        }
        Batch::try_new(operator.output.clone(), output)
    })
    .boxed_local())
}
