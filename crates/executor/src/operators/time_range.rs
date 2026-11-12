//! A `TimeRange` over a `Scan` or a tumbling-window chain: the rows of one
//! window on the time column, for a SQL table, a time series and a chain's
//! `window_end` alike. A time series' stale markers (the Prometheus
//! end-of-series NaN) are never rows; an ordinary NaN is. And a `TimeShift`
//! with an `@` anchor, which evaluates its input at the anchor's time.
use super::*;
use arrow::array::Array;
use planner_types::ir::operator::AtModifier;

impl Operator {
    /// The rows of `input` whose `time` lies in the window of a
    /// `TimeRange(width_ms)` over shifts summing to `end_offset_ms`: `(T -
    /// end_offset_ms - width_ms, T - end_offset_ms]` in a query run at `T`,
    /// `(window_end_ms - width_ms, window_end_ms]` in an ingestion run.
    /// With `sample_value`, a time series' value column, a stale marker is
    /// not a row. With `latest_per_series` (an instant window), only each
    /// series' latest row in the window is kept (of rows at one time, the
    /// last), and none when that row is a stale marker; a series is the
    /// values of those columns (its labels).
    pub fn time_range(
        input: SchemaRef,
        time: usize,
        end_offset_ms: i64,
        width_ms: i64,
        sample_value: Option<usize>,
        latest_per_series: Option<Vec<usize>>,
    ) -> Result<Self, Error> {
        if width_ms < 0 || plain(&input, time)?.0 != &DataType::Timestamp {
            return Err(invalid("a time range reads a timestamp column"));
        }
        if let Some(value) = sample_value {
            if plain(&input, value)?.0 != &DataType::Float64 {
                return Err(invalid("a time series' sample value is Float64"));
            }
        }
        if let Some(series) = &latest_per_series {
            if sample_value.is_none() {
                return Err(invalid("an instant window reads a time series"));
            }
            validate_groups(&input, series)?;
        }
        Ok(Self {
            kind: Kind::TimeRange {
                time,
                end_offset_ms,
                width_ms,
                sample_value,
                latest_per_series,
                tumbling_window_width_ms: None,
            },
            inputs: vec![input.clone()],
            output: input,
        })
    }

    /// This range window (not an instant window), each kept row's time
    /// replaced by the end of the tumbling window of `width_ms` it lies in,
    /// the windows counted back from the range's end `E`: `(E - (k+1) ·
    /// width_ms, E - k · width_ms]` ends at `E - k · width_ms`. A
    /// tumbling-window chain built at query time groups its rows by that
    /// end. Example: with `E` = 600,000 and windows of 60,000 ms, rows at
    /// 540,001 and 600,000 end at 600,000, a row at 540,000 at 540,000.
    pub fn with_tumbling_window_ends(mut self, width_ms: i64) -> Result<Self, Error> {
        let Kind::TimeRange {
            latest_per_series: None,
            tumbling_window_width_ms: slot,
            ..
        } = &mut self.kind
        else {
            return Err(invalid("tumbling-window ends need a range window"));
        };
        if width_ms <= 0 {
            return Err(invalid("a tumbling window has a positive width"));
        }
        *slot = Some(width_ms);
        Ok(self)
    }
}

impl Operator {
    /// A `TimeShift { offset_ms, at }`: its rows are its input's, which a
    /// run evaluates once, at the anchor's time minus `offset_ms`
    /// ([`anchored_context`]).
    pub fn evaluate_at(input: SchemaRef, at: AtModifier, offset_ms: i64) -> Result<Self, Error> {
        Ok(Self {
            kind: Kind::EvaluateAt { at, offset_ms },
            inputs: vec![input.clone()],
            output: input,
        })
    }
}

/// The context an `@`-anchored input runs in: a query run evaluated at the
/// anchor's time minus `offset_ms`. `@ <t>` pins `t`; `@ start()` and `@
/// end()` the request's bounds ([`RunContext::with_query_range`]), which an
/// instant query (a run without them) has both at its evaluation time, as
/// in Prometheus. An ingestion run has no anchor time.
pub(super) fn anchored_context(
    at: AtModifier,
    offset_ms: i64,
    context: &RunContext,
) -> Result<RunContext, Error> {
    let crate::runtime::Scope::Query { evaluation_time_ms } = context.scope else {
        return Err(invalid(
            "an @-anchored node is evaluated only at query time",
        ));
    };
    let (start, end) = context
        .query_range()
        .unwrap_or((evaluation_time_ms, evaluation_time_ms));
    let anchor = match at {
        AtModifier::Timestamp(ms) => ms,
        AtModifier::Start => start,
        AtModifier::End => end,
    };
    let time = anchor
        .checked_sub(offset_ms)
        .ok_or_else(|| invalid("anchored time overflows Int64"))?;
    Ok(context.at_evaluation_time(time))
}

/// An `@`-anchored node's rows: its input's, unchanged.
pub(super) fn execute_evaluate_at<'a>(
    mut inputs: Vec<Input<'a, Batch>>,
    context: RunContext,
) -> Result<OutputStream<'a, Batch>, Error> {
    let input = inputs
        .pop()
        .ok_or_else(|| invalid("anchored input missing"))?;
    Ok(input
        .map(move |batch| {
            if context.is_cancelled() {
                return Err(Error::Cancelled);
            }
            Ok((*batch?).clone())
        })
        .boxed_local())
}

/// `(start, end]` of a window `width_ms` wide in this run: ending
/// `end_offset_ms` before the evaluation time `T` in a query run; ending at
/// the run's `window_end_ms` in an ingestion run, which builds the window
/// the shifts select (the shifts say which window a query reads, the
/// ingestion run's window which one it builds).
pub(super) fn window(
    end_offset_ms: i64,
    width_ms: i64,
    context: &RunContext,
) -> Result<(i64, i64), Error> {
    let end = match context.scope {
        crate::runtime::Scope::Query {
            evaluation_time_ms, ..
        } => evaluation_time_ms
            .checked_sub(end_offset_ms)
            .ok_or_else(|| invalid("time range overflows Int64"))?,
        crate::runtime::Scope::Ingestion { window_end_ms, .. } => window_end_ms,
    };
    let start = end
        .checked_sub(width_ms)
        .ok_or_else(|| invalid("time range overflows Int64"))?;
    Ok((start, end))
}

pub(super) fn execute<'a>(
    operator: &'a Operator,
    mut inputs: Vec<Input<'a, Batch>>,
    context: RunContext,
) -> Result<OutputStream<'a, Batch>, Error> {
    let Kind::TimeRange {
        time,
        end_offset_ms,
        width_ms,
        sample_value,
        ref latest_per_series,
        tumbling_window_width_ms,
    } = operator.kind
    else {
        unreachable!()
    };
    let input = inputs
        .pop()
        .ok_or_else(|| invalid("time range input missing"))?;
    let (start, end) = window(end_offset_ms, width_ms, &context)?;
    // Whether each row is in the window and, for a range window, not a
    // stale marker.
    let kept =
        move |batch: &Batch, drop_stale: bool| -> Result<arrow::array::BooleanArray, Error> {
            let times = batch
                .column(time)
                .as_any()
                .downcast_ref::<arrow::array::TimestampMillisecondArray>()
                .ok_or_else(|| invalid("time range column is not a timestamp"))?;
            let values = match sample_value.filter(|_| drop_stale) {
                Some(value) => Some(
                    batch
                        .column(value)
                        .as_any()
                        .downcast_ref::<arrow::array::Float64Array>()
                        .ok_or_else(|| invalid("sample value is not Float64"))?,
                ),
                None => None,
            };
            Ok((0..batch.num_rows())
                .map(|row| {
                    let in_window = times.is_valid(row) && {
                        let t = times.value(row);
                        start < t && t <= end
                    };
                    let stale = values.is_some_and(|values| {
                        values.is_valid(row) && values.value(row).to_bits() == STALE_MARKER
                    });
                    Some(in_window && !stale)
                })
                .collect())
        };
    let Some(series) = latest_per_series else {
        return Ok(input
            .map(move |batch| {
                if context.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                let batch = batch?;
                let keep = kept(&batch, true)?;
                let batch = if keep.true_count() == batch.num_rows() {
                    (*batch).clone()
                } else {
                    batch.filter(&keep)?
                };
                match tumbling_window_width_ms {
                    Some(width_ms) => window_ends(batch, time, end, width_ms),
                    None => Ok(batch),
                }
            })
            .boxed_local());
    };
    let series = series.clone();
    Ok(futures::stream::once(async move {
        let input = collect_batch(input, &operator.inputs[0], &context).await?;
        // The rows in the window, stale markers included: a series whose
        // latest sample is one has ended.
        let in_window = input.batch.filter(&kept(&input.batch, false)?)?;
        let times = in_window
            .column(time)
            .as_any()
            .downcast_ref::<arrow::array::TimestampMillisecondArray>()
            .expect("checked timestamps")
            .clone();
        let mut table = super::group_table::GroupTable::new(series);
        let mut work = Cooperative::new(&context);
        let mut workspace = Workspace::new(&context)?;
        let series_of_row =
            chunked_group_ids(&mut table, &in_window, &mut workspace, 0, &mut work).await?;
        // Each series' latest row; of rows at one time, the last.
        let mut latest: Vec<Option<u32>> = vec![None; table.len()];
        for (row, &series) in series_of_row.iter().enumerate() {
            let slot = &mut latest[series as usize];
            if slot.is_none_or(|previous| times.value(row) >= times.value(previous as usize)) {
                *slot = Some(row as u32);
            }
        }
        workspace.set_bytes(table.memory_bytes() + latest.len() * 8)?;
        let latest = in_window.take(&arrow::array::UInt32Array::from(
            table
                .groups_in_key_order()
                .into_iter()
                .filter_map(|series| latest[series as usize])
                .collect::<Vec<_>>(),
        ))?;
        // Then a series whose latest sample is a stale marker has no row.
        let keep = kept(&latest, true)?;
        latest.filter(&keep)
    })
    .boxed_local())
}

/// `batch` (its rows in `(.., end]`) with each row's `time` replaced by the
/// end of its tumbling window of `width_ms` counted back from `end`
/// ([`Operator::with_tumbling_window_ends`]).
fn window_ends(batch: Batch, time: usize, end: i64, width_ms: i64) -> Result<Batch, Error> {
    let times = batch
        .column(time)
        .as_any()
        .downcast_ref::<arrow::array::TimestampMillisecondArray>()
        .ok_or_else(|| invalid("time range column is not a timestamp"))?;
    // `end - t` is in `[k · width_ms, (k+1) · width_ms)` for the window
    // ending at `end - k · width_ms`.
    let ends: arrow::array::TimestampMillisecondArray = times
        .iter()
        .map(|t| t.map(|t| end - (end - t) / width_ms * width_ms))
        .collect();
    let rows = batch.num_rows();
    let mut arrays = Vec::with_capacity(batch.schema().fields.len());
    let mut states = Vec::with_capacity(arrays.capacity());
    for column in 0..batch.schema().fields.len() {
        if column == time {
            arrays.push(std::sync::Arc::new(ends.clone()) as arrow::array::ArrayRef);
            states.push(None);
        } else {
            arrays.push(batch.column(column).clone());
            states.push(batch.state_column(column).cloned());
        }
    }
    Batch::from_parts(batch.schema().clone(), arrays, states, rows)
}
