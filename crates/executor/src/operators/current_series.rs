//! A bounded instant-vector snapshot: select latest before removing stale markers.
use super::*;

impl Operator {
    pub fn current_series(
        input: SchemaRef,
        identity: usize,
        coordinate: usize,
        value: usize,
        lookback_ms: i64,
    ) -> Result<Self, Error> {
        if lookback_ms <= 0
            || plain(&input, identity)? != (&DataType::Utf8, false)
            || plain(&input, coordinate)? != (&DataType::Timestamp, false)
            || plain(&input, value)? != (&DataType::Float64, false)
        {
            return Err(invalid("invalid current-series input contract"));
        }
        Ok(Self {
            kind: Kind::CurrentSeries {
                identity,
                coordinate,
                value,
                lookback_ms,
            },
            inputs: vec![input.clone()],
            output: input,
        })
    }
}

pub(super) fn execute<'a>(
    operator: &'a Operator,
    mut inputs: Vec<Input<'a, Batch>>,
    context: RunContext,
) -> Result<OutputStream<'a, Batch>, Error> {
    let Kind::CurrentSeries {
        identity,
        coordinate,
        value,
        lookback_ms,
    } = operator.kind
    else {
        unreachable!()
    };
    let input = inputs
        .pop()
        .ok_or_else(|| invalid("current-series input missing"))?;
    let (start, end) = window(lookback_ms, &context)?;
    Ok(futures::stream::once(async move {
        let input = collect_batch(input, &operator.inputs[0], &context).await?;
        let batch = &input.batch;
        let times = batch
            .column(coordinate)
            .as_any()
            .downcast_ref::<arrow::array::TimestampMillisecondArray>()
            .expect("timestamps")
            .clone();
        let values = batch
            .column(value)
            .as_any()
            .downcast_ref::<arrow::array::Float64Array>()
            .expect("Float64 values")
            .clone();
        // Each series' latest row in the window, by its identity.
        let mut table = super::group_table::GroupTable::new(vec![identity]);
        let mut work = Cooperative::new(&context);
        let mut workspace = Workspace::new(&context)?;
        let series = chunked_group_ids(&mut table, batch, &mut workspace, 0, &mut work).await?;
        let mut latest: Vec<Option<u32>> = vec![None; table.len()];
        for (row, &series) in series.iter().enumerate() {
            let timestamp = times.value(row);
            if timestamp <= start || timestamp > end {
                continue;
            }
            let slot = &mut latest[series as usize];
            if let Some(previous) = *slot {
                let previous_time = times.value(previous as usize);
                if timestamp < previous_time {
                    continue;
                }
                if timestamp == previous_time {
                    if values.value(row).to_bits() != values.value(previous as usize).to_bits() {
                        return Err(invalid("conflicting samples for one series timestamp"));
                    }
                    continue;
                }
            }
            *slot = Some(row as u32);
        }
        work.checkpoint_rows(batch.num_rows()).await?;
        workspace.set_bytes(table.memory_bytes() + latest.len() * 8)?;
        // In identity key order, without stale markers.
        let rows = table
            .groups_in_key_order()
            .into_iter()
            .filter_map(|series| latest[series as usize])
            .filter(|&row| values.value(row as usize).to_bits() != 0x7ff0_0000_0000_0002)
            .collect::<Vec<_>>();
        let selected = batch.take(&arrow::array::UInt32Array::from(rows))?;
        let stamped = std::sync::Arc::new(arrow::array::TimestampMillisecondArray::from(vec![
            end;
            selected.num_rows()
        ]));
        selected.with_plain_column(coordinate, stamped)
    })
    .boxed_local())
}

pub(super) fn window(lookback_ms: i64, context: &RunContext) -> Result<(i64, i64), Error> {
    let end = match context.scope {
        crate::runtime::Scope::Query {
            evaluation_time_ms, ..
        } => evaluation_time_ms,
        crate::runtime::Scope::Ingestion { window_end_ms, .. } => window_end_ms,
    };
    let start = end
        .checked_sub(lookback_ms)
        .ok_or_else(|| invalid("current-series window overflows"))?;
    if let crate::runtime::Scope::Ingestion {
        window_start_ms, ..
    } = context.scope
    {
        if window_start_ms != start {
            return Err(invalid(
                "current-series maintenance window differs from lookback",
            ));
        }
    }
    Ok((start, end))
}

pub(super) fn validate_context(operator: &Operator, context: &RunContext) -> Result<(), Error> {
    if let Kind::CurrentSeries { lookback_ms, .. } = operator.kind {
        window(lookback_ms, context)?;
    }
    Ok(())
}
