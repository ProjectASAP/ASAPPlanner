use super::*;
impl Operator {
    pub fn sort(input: Schema, keys: Vec<SortKey>, groups: Vec<usize>) -> Result<Self, Error> {
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
#[derive(Clone, Debug)]
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
    let output = operator.output.clone();
    let input = inputs.pop().ok_or_else(|| invalid("input missing"))?;
    Ok(futures::stream::once(async move {
        let (rows, _memory) = collect_rows(input, &context).await?;
        let result = match &operator.kind {
            Kind::Sort { keys, groups } => {
                let mut grouped = BTreeMap::<Vec<Vec<u8>>, Vec<Vec<Value>>>::new();
                let mut work = Cooperative::new(&context);
                let mut workspace = Workspace::new(&context)?;
                for row in rows {
                    work.checkpoint().await?;
                    for key in keys {
                        if matches!(row[key.column], Value::Map(_)) && nested_nan(&row[key.column])
                        {
                            return Err(invalid("NaN in collection sort key"));
                        }
                    }
                    let key = group_key(&row, groups)?;
                    workspace.grow(std::mem::size_of::<Vec<Value>>())?;
                    if !grouped.contains_key(&key) {
                        workspace.grow(key_bytes(&key))?;
                    }
                    grouped.entry(key).or_default().push(row);
                }
                let mut result = Vec::new();
                for rows in grouped.into_values() {
                    let rows =
                        cooperative_sort(rows, |a, b| compare_rows(a, b, keys), &context).await?;
                    result.extend(rows);
                }
                result
            }
            _ => unreachable!(),
        };
        Batch::try_new(output, result)
    })
    .boxed_local())
}

fn compare_rows(a: &[Value], b: &[Value], keys: &[SortKey]) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    for key in keys {
        let (a, b) = (&a[key.column], &b[key.column]);
        let order = match (a, b) {
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
        };
        if order != Equal {
            return order;
        }
    }
    Equal
}
fn nested_nan(value: &Value) -> bool {
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
