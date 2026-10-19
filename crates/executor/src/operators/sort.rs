use super::*;
impl Operator {
    pub fn sort(input: SchemaRef, keys: Vec<SortKey>, groups: Vec<usize>) -> Result<Self, Error> {
        validate_groups(&input, &groups)?;
        for key in &keys {
            if !ordered(plain(&input, key.column)?.0) {
                return Err(invalid("unsupported sort type"));
            }
        }
        Ok(Self {
            kind: Kind::Sort { keys, groups },
            inputs: vec![input.clone()],
            output: input,
        })
    }
}
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct SortKey {
    pub column: usize,
    pub descending: bool,
    pub nulls_first: bool,
}
pub(super) fn execute<'a>(
    operator: &'a Operator,
    mut inputs: Vec<Input<'a, Batch>>,
    context: RunContext,
) -> Result<OutputStream<'a, Batch>, Error> {
    let input = inputs.pop().ok_or_else(|| invalid("input missing"))?;
    let Kind::Sort { keys, groups } = &operator.kind else {
        unreachable!()
    };
    Ok(futures::stream::once(async move {
        let input = collect_batch(input, &operator.inputs[0], &context).await?;
        let batch = &input.batch;
        let rows = batch.num_rows();
        // The sort keys' values, column by column.
        let key_values = keys
            .iter()
            .map(|key| decoded(batch, key.column))
            .collect::<Result<Vec<_>, _>>()?;
        for values in &key_values {
            if values
                .iter()
                .any(|value| matches!(value, Value::Map(_)) && nested_nan(value))
            {
                return Err(invalid("NaN in collection sort key"));
            }
        }
        // Groups in key order, then rows by the sort keys; ties keep input
        // order (a stable sort).
        let mut table = super::group_table::GroupTable::new(groups.clone());
        let ids = table.group_ids(batch)?;
        let mut rank = vec![0u32; table.len()];
        for (position, group) in table.groups_in_key_order().into_iter().enumerate() {
            rank[group as usize] = position as u32;
        }
        let mut workspace = Workspace::new(&context)?;
        workspace.set_bytes(
            table.memory_bytes()
                + rows * 2 * std::mem::size_of::<u32>()
                + key_values.iter().flatten().map(Value::bytes).sum::<usize>(),
        )?;
        let order = cooperative_sort(
            (0..rows as u32).collect::<Vec<_>>(),
            |&a, &b| {
                rank[ids[a as usize] as usize]
                    .cmp(&rank[ids[b as usize] as usize])
                    .then_with(|| {
                        keys.iter()
                            .zip(&key_values)
                            .map(|(key, values)| {
                                compare_key(&values[a as usize], &values[b as usize], key)
                            })
                            .find(|order| order.is_ne())
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
            },
            &context,
        )
        .await?;
        batch.take(&arrow::array::UInt32Array::from(order))
    })
    .boxed_local())
}

#[cfg(test)]
pub(super) fn compare_rows(a: &[Value], b: &[Value], keys: &[SortKey]) -> std::cmp::Ordering {
    for key in keys {
        let order = compare_key(&a[key.column], &b[key.column], key);
        if order.is_ne() {
            return order;
        }
    }
    std::cmp::Ordering::Equal
}

/// The order of two values of one sort key: NULLs first or last, NaN after
/// every number, then the key's direction.
fn compare_key(a: &Value, b: &Value, key: &SortKey) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    {
        match (a, b) {
            (Value::Null, Value::Null) => Equal,
            (Value::Null, _) => {
                if key.nulls_first {
                    Less
                } else {
                    Greater
                }
            }
            (_, Value::Null) => {
                if key.nulls_first {
                    Greater
                } else {
                    Less
                }
            }
            (Value::Float64(a), Value::Float64(b)) if a.is_nan() || b.is_nan() => {
                match (a.is_nan(), b.is_nan()) {
                    (true, true) => Equal,
                    (true, false) => Greater,
                    _ => Less,
                }
            }
            _ => {
                let order = a.compare(b).expect("bound ordered types");
                if key.descending {
                    order.reverse()
                } else {
                    order
                }
            }
        }
    }
}
pub(super) fn nested_nan(value: &Value) -> bool {
    match value {
        Value::Float64(value) => value.is_nan(),
        Value::Map(values) => values
            .iter()
            .any(|(key, value)| nested_nan(key) || nested_nan(value)),
        Value::List(values) | Value::Struct(values) => values.iter().any(nested_nan),
        _ => false,
    }
}
/// Stable in-memory merge sort with bounded synchronous chunks. Scratch storage
/// is reserved before allocation; comparisons yield between merge steps.
pub(super) async fn cooperative_sort<T>(
    rows: Vec<T>,
    compare: impl Fn(&T, &T) -> std::cmp::Ordering,
    context: &RunContext,
) -> Result<Vec<T>, Error> {
    use std::collections::VecDeque;
    let bytes = rows
        .len()
        .checked_mul(std::mem::size_of::<T>() + std::mem::size_of::<VecDeque<T>>())
        .and_then(|n| n.checked_mul(3))
        .ok_or(Error::MemoryLimit)?;
    let _scratch = context.reserve(bytes)?;
    let mut work = Cooperative::new(context);
    let mut rows = rows.into_iter();
    let mut runs = VecDeque::new();
    loop {
        work.checkpoint().await?;
        let mut chunk = rows.by_ref().take(256).collect::<Vec<_>>();
        if chunk.is_empty() {
            break;
        }
        chunk.sort_by(&compare);
        runs.push_back(VecDeque::from(chunk));
    }
    // Merge adjacent runs in rounds to preserve ties in original input order.
    while runs.len() > 1 {
        let mut next = VecDeque::new();
        while let Some(mut left) = runs.pop_front() {
            let Some(mut right) = runs.pop_front() else {
                next.push_back(left);
                break;
            };
            let mut merged = VecDeque::with_capacity(left.len() + right.len());
            while !left.is_empty() || !right.is_empty() {
                work.checkpoint().await?;
                let take_left = match (left.front(), right.front()) {
                    (Some(a), Some(b)) => !compare(a, b).is_gt(),
                    (Some(_), None) => true,
                    _ => false,
                };
                merged.push_back(if take_left {
                    left.pop_front().unwrap()
                } else {
                    right.pop_front().unwrap()
                });
            }
            next.push_back(merged);
        }
        runs = next;
    }
    Ok(runs.pop_front().unwrap_or_default().into())
}
