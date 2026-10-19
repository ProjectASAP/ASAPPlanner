//! The row-at-a-time `CurrentSeries` (reference for tests).
use super::super::current_series::window;
use super::super::*;

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
    let output = operator.output.clone();
    let (start, end) = window(lookback_ms, &context)?;
    Ok(futures::stream::once(async move {
        let (rows, _memory) = collect_rows(input, &context).await?;
        let mut latest = BTreeMap::<Vec<u8>, usize>::new();
        let mut work = Cooperative::new(&context);
        let mut workspace = Workspace::new(&context)?;
        for (index, row) in rows.iter().enumerate() {
            work.checkpoint().await?;
            let Value::Timestamp(timestamp) = row[coordinate] else {
                unreachable!()
            };
            if timestamp <= start || timestamp > end {
                continue;
            }
            let key = row[identity].key()?;
            if let Some(&previous) = latest.get(&key) {
                let Value::Timestamp(previous_time) = rows[previous][coordinate] else {
                    unreachable!()
                };
                if timestamp < previous_time {
                    continue;
                }
                if timestamp == previous_time {
                    let (Value::Float64(a), Value::Float64(b)) =
                        (&row[value], &rows[previous][value])
                    else {
                        unreachable!()
                    };
                    if a.to_bits() != b.to_bits() {
                        return Err(invalid("conflicting samples for one series timestamp"));
                    }
                    continue;
                }
            } else {
                workspace.grow(64 + key.len())?;
            }
            latest.insert(key, index);
        }
        let mut result = Vec::new();
        for index in latest.into_values() {
            work.checkpoint().await?;
            let Value::Float64(sample) = rows[index][value] else {
                unreachable!()
            };
            if sample.to_bits() == 0x7ff0_0000_0000_0002 {
                continue;
            }
            workspace.grow(row_bytes(&rows[index]))?;
            let mut row = rows[index].clone();
            row[coordinate] = Value::Timestamp(end);
            result.push(row);
        }
        Batch::try_new(output, result)
    })
    .boxed_local())
}
