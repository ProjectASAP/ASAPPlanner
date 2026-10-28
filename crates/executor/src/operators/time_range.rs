//! A `TimeRange` over a `Scan`: the rows of one window on the time column,
//! for a SQL table and a time series alike. A time series' stale markers
//! (the Prometheus end-of-series NaN) are never rows; an ordinary NaN is.
use super::*;
use arrow::array::Array;

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
            },
            inputs: vec![input.clone()],
            output: input,
        })
    }
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
                if keep.true_count() == batch.num_rows() {
                    return Ok((*batch).clone());
                }
                batch.filter(&keep)
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
