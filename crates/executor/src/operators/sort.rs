use super::*;
impl Operator {
    /// Sort `input`'s rows by `keys` within each group of the `groups`
    /// columns, groups in key order. `fetch` keeps only the first `fetch`
    /// rows of each group, in sort order: `topk by (job) (3, m)` is a sort
    /// by value descending, grouped by `job`, with `fetch` `Some(3)`, which
    /// holds 3 rows per job instead of every input row. `None` sorts every
    /// row.
    pub fn sort(
        input: SchemaRef,
        keys: Vec<SortKey>,
        groups: Vec<usize>,
        fetch: Option<u64>,
    ) -> Result<Self, Error> {
        validate_groups(&input, &groups)?;
        for key in &keys {
            if !ordered(plain(&input, key.column)?.0) {
                return Err(invalid("unsupported sort type"));
            }
        }
        Ok(Self {
            kind: Kind::Sort {
                keys,
                groups,
                fetch,
            },
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
    let Kind::Sort {
        keys,
        groups,
        fetch,
    } = &operator.kind
    else {
        unreachable!()
    };
    if let Some(fetch) = *fetch {
        return Ok(futures::stream::once(sort_with_fetch(
            input,
            &operator.inputs[0],
            keys,
            groups,
            fetch,
            context,
        ))
        .boxed_local());
    }
    Ok(futures::stream::once(async move {
        let input = collect_batch(input, &operator.inputs[0], &context).await?;
        let batch = &input.batch;
        let rows = batch.num_rows();
        // The sort keys' values, column by column.
        let key_values = sort_key_values(batch, keys)?;
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
                        compare_rows(
                            keys,
                            key_values.iter().map(|values| &values[a as usize]),
                            key_values.iter().map(|values| &values[b as usize]),
                        )
                    })
            },
            &context,
        )
        .await?;
        batch.take(&arrow::array::UInt32Array::from(order))
    })
    .boxed_local())
}

/// The sort keys' values of `batch`, column by column; an error for a NaN
/// inside a collection key, which has no order.
fn sort_key_values(batch: &Batch, keys: &[SortKey]) -> Result<Vec<Vec<Value>>, Error> {
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
    Ok(key_values)
}

/// The order of two rows by `keys`, given each row's key values in order:
/// the first key that differs decides.
fn compare_rows<'v>(
    keys: &[SortKey],
    a: impl Iterator<Item = &'v Value>,
    b: impl Iterator<Item = &'v Value>,
) -> std::cmp::Ordering {
    keys.iter()
        .zip(a.zip(b))
        .map(|(key, (a, b))| compare_key(a, b, key))
        .find(|order| order.is_ne())
        .unwrap_or(std::cmp::Ordering::Equal)
}

/// One row a sort with `fetch` holds: its key values, its position in the
/// input (which breaks ties, as the stable sort keeps input order) and
/// where its row is held: a chunk of held rows, and the row of the input
/// batch that chunk was taken from.
struct HeldRow<'k> {
    keys: Vec<Value>,
    input_position: u64,
    chunk: u32,
    row: u32,
    sort: &'k [SortKey],
}

impl PartialEq for HeldRow<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for HeldRow<'_> {}
impl PartialOrd for HeldRow<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
/// Sort order: a row that sorts later is greater, so a group's max-heap
/// has its last kept row on top.
impl Ord for HeldRow<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        compare_rows(self.sort, self.keys.iter(), other.keys.iter())
            .then(self.input_position.cmp(&other.input_position))
    }
}

/// Rows a sort with `fetch` holds, taken from one input batch: `rows` are
/// that batch's rows `input_rows` (ascending). A row a later batch
/// displaces stays until [`compact`] rewrites the kept rows into one chunk.
struct HeldChunk {
    rows: Batch,
    input_rows: Vec<u32>,
}

/// The first `fetch` rows of each group of `input` by `keys`, groups in key
/// order, ties in input order, as a full sort and a per-group limit give
/// them. Each group keeps a heap of at most `fetch` rows, read a batch at a
/// time, so it holds `groups × fetch` rows (plus displaced rows, until they
/// are compacted away) instead of its whole input.
async fn sort_with_fetch(
    mut input: Input<'_, Batch>,
    schema: &SchemaRef,
    keys: &[SortKey],
    groups: &[usize],
    fetch: u64,
    context: RunContext,
) -> Result<Batch, Error> {
    use std::collections::BinaryHeap;
    let mut table = super::group_table::GroupTable::new(groups.to_vec());
    let mut heaps: Vec<BinaryHeap<HeldRow>> = Vec::new();
    let mut chunks: Vec<HeldChunk> = Vec::new();
    let mut workspace = Workspace::new(&context)?;
    let mut work = Cooperative::new(&context);
    let mut input_position = 0u64;
    let mut kept_rows = 0usize;
    let mut key_bytes = 0usize;
    let held_bytes = |chunks: &[HeldChunk], key_bytes: usize, kept_rows: usize| {
        chunks
            .iter()
            .map(|chunk| chunk.rows.memory_bytes() + 4 * chunk.input_rows.len())
            .sum::<usize>()
            + key_bytes
            + kept_rows * std::mem::size_of::<HeldRow>()
    };
    let fetch = usize::try_from(fetch).unwrap_or(usize::MAX);
    while let Some(batch) = input.next().await {
        let batch = batch?;
        work.checkpoint_rows(batch.num_rows()).await?;
        if fetch == 0 {
            continue;
        }
        let held = held_bytes(&chunks, key_bytes, kept_rows);
        let ids = chunked_group_ids(&mut table, &batch, &mut workspace, held, &mut work).await?;
        heaps.resize_with(table.len(), BinaryHeap::new);
        let key_values = sort_key_values(&batch, keys)?;
        let chunk = chunks.len() as u32;
        // Whether each row of `batch` is still kept.
        let mut kept = vec![false; batch.num_rows()];
        for (row, &group) in ids.iter().enumerate() {
            let heap = &mut heaps[group as usize];
            let position = input_position + row as u64;
            let sorts_before = |last: &HeldRow| {
                compare_rows(
                    keys,
                    key_values.iter().map(|values| &values[row]),
                    last.keys.iter(),
                )
                .then(position.cmp(&last.input_position))
                .is_lt()
            };
            if heap.len() >= fetch && !heap.peek().is_some_and(sorts_before) {
                continue;
            }
            let held = HeldRow {
                keys: key_values
                    .iter()
                    .map(|values| values[row].clone())
                    .collect(),
                input_position: position,
                chunk,
                row: row as u32,
                sort: keys,
            };
            key_bytes += held.keys.iter().map(Value::bytes).sum::<usize>();
            heap.push(held);
            kept[row] = true;
            kept_rows += 1;
            if heap.len() > fetch {
                let displaced = heap.pop().expect("a full heap");
                key_bytes -= displaced.keys.iter().map(Value::bytes).sum::<usize>();
                kept_rows -= 1;
                if displaced.chunk == chunk {
                    kept[displaced.row as usize] = false;
                }
            }
        }
        input_position += batch.num_rows() as u64;
        let input_rows: Vec<u32> = (0..batch.num_rows() as u32)
            .filter(|&row| kept[row as usize])
            .collect();
        if !input_rows.is_empty() {
            chunks.push(HeldChunk {
                rows: batch.take(&arrow::array::UInt32Array::from(input_rows.clone()))?,
                input_rows,
            });
        }
        let chunk_rows = chunks.iter().map(|c| c.input_rows.len()).sum::<usize>();
        if chunk_rows > 2 * kept_rows.max(1) {
            compact(schema, &mut chunks, &mut heaps)?;
        }
        workspace.set_bytes(table.memory_bytes() + held_bytes(&chunks, key_bytes, kept_rows))?;
    }
    compact(schema, &mut chunks, &mut heaps)?;
    // Groups in key order, each group's rows in sort order.
    let mut order = Vec::with_capacity(kept_rows);
    for group in table.groups_in_key_order() {
        if let Some(heap) = heaps.get_mut(group as usize) {
            order.extend(
                std::mem::take(heap)
                    .into_sorted_vec()
                    .iter()
                    .map(|held| held.row),
            );
        }
    }
    match chunks.pop() {
        Some(kept) => kept.rows.take(&arrow::array::UInt32Array::from(order)),
        None => Batch::try_new(schema.clone(), Vec::new()),
    }
}

/// Rewrite the rows `heaps` keep into one chunk, in heap order, dropping
/// displaced rows, and point each kept row at its row there.
fn compact(
    schema: &SchemaRef,
    chunks: &mut Vec<HeldChunk>,
    heaps: &mut [std::collections::BinaryHeap<HeldRow>],
) -> Result<(), Error> {
    if chunks.is_empty() {
        return Ok(());
    }
    let mut offsets = Vec::with_capacity(chunks.len());
    let mut total = 0u32;
    for chunk in chunks.iter() {
        offsets.push(total);
        total += chunk.input_rows.len() as u32;
    }
    let mut rows = Vec::new();
    for heap in heaps.iter_mut() {
        let mut held = std::mem::take(heap).into_vec();
        for row in &mut held {
            let chunk = &chunks[row.chunk as usize];
            let index = chunk
                .input_rows
                .binary_search(&row.row)
                .expect("a kept row is in its chunk");
            rows.push(offsets[row.chunk as usize] + index as u32);
            row.chunk = 0;
            row.row = rows.len() as u32 - 1;
        }
        *heap = std::collections::BinaryHeap::from(held);
    }
    let all = Batch::concat(
        schema,
        &chunks
            .iter()
            .map(|chunk| chunk.rows.clone())
            .collect::<Vec<_>>(),
    )?;
    *chunks = vec![HeldChunk {
        rows: all.take(&arrow::array::UInt32Array::from(rows.clone()))?,
        input_rows: (0..rows.len() as u32).collect(),
    }];
    Ok(())
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
