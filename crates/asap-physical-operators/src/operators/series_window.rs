//! PromQL per-series evaluation over the samples before an evaluation instant.
use super::*;
use planner_types::pre_asap::AggIntent;

/// A PromQL subquery grid: every multiple of `step_ms` in
/// `(T - offset_ms - range_ms, T - offset_ms]`, where `T` is the query time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SubquerySteps {
    pub range_ms: i64,
    pub step_ms: i64,
    pub offset_ms: i64,
}

const STALE_MARKER: u64 = 0x7ff0_0000_0000_0002;
const MAX_SUBQUERY_STEPS: i64 = 100_000;

impl Operator {
    /// Evaluate each series at instant `t` over its samples in
    /// `(t - offset_ms - range_ms, t - offset_ms]`. `t` is the query time, or
    /// each step of `steps`. `function: None` is instant selection: the latest
    /// sample, absent if it is a stale marker. Range functions ignore stale
    /// markers. A series is every column except the time and `value` columns.
    /// Output rows keep the input schema, with time `t` and the result value.
    pub fn series_window(
        input: Schema,
        function: Option<AggIntent<ColumnRef>>,
        range_ms: i64,
        offset_ms: i64,
        steps: Option<SubquerySteps>,
    ) -> Result<Self, Error> {
        let coordinate = input
            .time_index
            .ok_or_else(|| invalid("series window requires a time column"))?;
        let values = input
            .fields
            .iter()
            .enumerate()
            .filter(|(_, f)| f.name == "value")
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        let [value] = values.as_slice() else {
            return Err(invalid("series window requires one value column"));
        };
        if plain(&input, coordinate)? != (&DataType::Timestamp, false)
            || plain(&input, *value)? != (&DataType::Float64, false)
        {
            return Err(invalid(
                "series window requires non-null time and Float64 value",
            ));
        }
        if range_ms <= 0 || steps.is_some_and(|s| s.range_ms <= 0 || s.step_ms <= 0) {
            return Err(invalid("series window ranges and steps must be positive"));
        }
        // Bounds per-run work independently of the data, as the backend's grid does.
        if steps.is_some_and(|s| s.range_ms / s.step_ms > MAX_SUBQUERY_STEPS) {
            return Err(invalid("subquery exceeds 100000 steps"));
        }
        if !matches!(
            function,
            None | Some(
                AggIntent::Rate
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
                    | AggIntent::Quantile { col: None, .. }
            )
        ) {
            return Err(invalid("unsupported PromQL range function"));
        }
        Ok(Self {
            kind: Kind::SeriesWindow {
                function: function.map(Box::new),
                coordinate,
                value: *value,
                range_ms,
                offset_ms,
                steps,
            },
            inputs: vec![input.clone()],
            output: input,
        })
    }
}

/// The first and last evaluation instants and the step between them. The grid
/// is iterated, not allocated: its size depends only on the query.
fn evaluation_times(
    context: &RunContext,
    steps: Option<SubquerySteps>,
) -> Result<(i64, i64, i64), Error> {
    let crate::runtime::Scope::Query {
        evaluation_time_ms, ..
    } = context.scope
    else {
        return Err(invalid("series window requires a query evaluation time"));
    };
    let Some(steps) = steps else {
        return Ok((evaluation_time_ms, evaluation_time_ms, 1));
    };
    let overflow = || invalid("subquery grid overflows");
    let end = evaluation_time_ms
        .checked_sub(steps.offset_ms)
        .ok_or_else(overflow)?;
    let start = end.checked_sub(steps.range_ms).ok_or_else(overflow)?;
    let first = (start.div_euclid(steps.step_ms) + 1)
        .checked_mul(steps.step_ms)
        .ok_or_else(overflow)?;
    Ok((first, end, steps.step_ms))
}

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
        steps,
    } = &operator.kind
    else {
        unreachable!()
    };
    let (coordinate, value) = (*coordinate, *value);
    let input = inputs
        .pop()
        .ok_or_else(|| invalid("series window input missing"))?;
    let times = evaluation_times(&context, *steps)?;
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
            let end = time.checked_sub(*offset_ms).ok_or_else(overflow)?;
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
                            match aggregate::temporal::window_value(intent, &fresh, start, end)? {
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

pub(super) fn validate_context(operator: &Operator, context: &RunContext) -> Result<(), Error> {
    if let Kind::SeriesWindow { steps, .. } = operator.kind {
        evaluation_times(context, steps)?;
    }
    Ok(())
}
