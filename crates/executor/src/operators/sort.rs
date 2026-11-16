use super::*;
use arrow::array::{Array, ArrayRef};
use std::cmp::Ordering;
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
    keys.iter()
        .map(|key| key_column(batch, key.column))
        .collect()
}

/// The values of sort key column `column` of `batch`; an error for a NaN
/// inside a collection key (`{a: NaN}`), which has no order.
fn key_column(batch: &Batch, column: usize) -> Result<Vec<Value>, Error> {
    let values = decoded(batch, column)?;
    if values
        .iter()
        .any(|value| matches!(value, Value::Map(_)) && nested_nan(value))
    {
        return Err(invalid("NaN in collection sort key"));
    }
    Ok(values)
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

/// The sort order of two rows, given the order of their key values: ties
/// by input position, as the stable sort keeps input order. A row that
/// sorts later is greater.
fn sort_order(key_order: Ordering, a_position: u64, b_position: u64) -> Ordering {
    key_order.then(a_position.cmp(&b_position))
}

/// One row a sort with `fetch` holds: its key values, its position in the
/// input (which breaks ties) and where its row is held: row `row` of chunk
/// `chunk`, or, while `chunk` is the number of chunks (the index the batch
/// being read will take), row `row` of that batch.
struct HeldRow {
    keys: Box<[Value]>,
    input_position: u64,
    chunk: u32,
    row: u32,
}

/// Add `row` to `heap`, a max-heap in a `Vec` by `order` (the row that sorts
/// last on top).
fn heap_push(
    heap: &mut Vec<HeldRow>,
    row: HeldRow,
    order: &impl Fn(&HeldRow, &HeldRow) -> Ordering,
) {
    heap.push(row);
    let mut child = heap.len() - 1;
    while child > 0 {
        let parent = (child - 1) / 2;
        if order(&heap[child], &heap[parent]).is_le() {
            break;
        }
        heap.swap(child, parent);
        child = parent;
    }
}

/// Restore `heap` (see [`heap_push`]) after its top row was overwritten in
/// place by a row that sorts earlier: one sift down, where a push then a pop
/// reorder twice and allocate a new row.
fn heap_sift_down_top(heap: &mut [HeldRow], order: &impl Fn(&HeldRow, &HeldRow) -> Ordering) {
    let mut parent = 0;
    loop {
        let (left, right) = (2 * parent + 1, 2 * parent + 2);
        let mut largest = parent;
        if left < heap.len() && order(&heap[left], &heap[largest]).is_gt() {
            largest = left;
        }
        if right < heap.len() && order(&heap[right], &heap[largest]).is_gt() {
            largest = right;
        }
        if largest == parent {
            return;
        }
        heap.swap(parent, largest);
        parent = largest;
    }
}

/// Row `i` of `left` against row `j` of `right` by one sort key, both arrays
/// of the key's column, in [`compare_key`]'s order read straight from the
/// arrays: NULLs first or last, NaN after every number in either direction,
/// `-0` equal to `0`. A map key is decoded and compared by value, as Arrow's
/// map order differs inside the map: Arrow puts `{a: NULL}` before `{a: 1}`
/// and `{a: -0}` before `{a: 0}`; [`Value::compare`] puts a NULL value last
/// and `-0` equal to `0`.
fn key_comparator(
    left: &ArrayRef,
    right: &ArrayRef,
    dtype: &DataType,
    key: &SortKey,
) -> Result<arrow::array::DynComparator, Error> {
    use arrow::array::{AsArray, DynComparator};
    use arrow::datatypes::Float64Type;
    if matches!(dtype, DataType::Map { .. }) {
        let depth = crate::values::arrow_columns::ColumnDepth::BatchColumn;
        let left = crate::values::arrow_columns::decode(left.as_ref(), dtype, depth);
        let right = crate::values::arrow_columns::decode(right.as_ref(), dtype, depth);
        let key = key.clone();
        return Ok(Box::new(move |i, j| compare_key(&left[i], &right[j], &key)));
    }
    let values = arrow::array::make_comparator(left, right, Default::default())
        .map_err(|error| invalid(&error.to_string()))?;
    let (left_nulls, right_nulls) = (left.logical_nulls(), right.logical_nulls());
    let floats = left
        .as_primitive_opt::<Float64Type>()
        .cloned()
        .zip(right.as_primitive_opt::<Float64Type>().cloned());
    let SortKey {
        descending,
        nulls_first,
        ..
    } = *key;
    let compare: DynComparator = Box::new(move |i, j| {
        let left_null = left_nulls.as_ref().is_some_and(|nulls| nulls.is_null(i));
        let right_null = right_nulls.as_ref().is_some_and(|nulls| nulls.is_null(j));
        match (left_null, right_null) {
            (true, true) => return Ordering::Equal,
            (true, false) => {
                return if nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (false, true) => {
                return if nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (false, false) => {}
        }
        let order = match &floats {
            Some((left, right)) => {
                let (a, b) = (left.value(i), right.value(j));
                if a.is_nan() || b.is_nan() {
                    return a.is_nan().cmp(&b.is_nan());
                }
                a.partial_cmp(&b).expect("neither is NaN")
            }
            None => values(i, j),
        };
        if descending {
            order.reverse()
        } else {
            order
        }
    });
    Ok(compare)
}

/// The sort-key values of the rows of one batch a sort with `fetch` keeps,
/// decoded only for those rows: a number, time, date or bool key reads its
/// cell, a text key decodes its column once, at the first kept row. A map
/// key decodes its column when the batch is read, as the full sort does, so
/// a NaN inside any row's map is an error, kept or not.
struct KeptKeyValues<'b> {
    batch: &'b Batch,
    keys: &'b [SortKey],
    decoded: Vec<Option<Vec<Value>>>,
}

impl<'b> KeptKeyValues<'b> {
    fn new(batch: &'b Batch, keys: &'b [SortKey]) -> Result<Self, Error> {
        let decoded = keys
            .iter()
            .map(|key| match plain(batch.schema(), key.column)?.0 {
                DataType::Map { .. } => key_column(batch, key.column).map(Some),
                _ => Ok(None),
            })
            .collect::<Result<_, Error>>()?;
        Ok(Self {
            batch,
            keys,
            decoded,
        })
    }

    /// Row `row`'s key values, in a new allocation.
    fn row(&mut self, row: usize) -> Result<Box<[Value]>, Error> {
        let mut values = vec![Value::Null; self.keys.len()].into_boxed_slice();
        self.write(row, &mut values)?;
        Ok(values)
    }

    /// Write row `row`'s key values over `values`, one per key: a held row
    /// that is displaced passes its own, so a full group allocates nothing.
    fn write(&mut self, row: usize, values: &mut [Value]) -> Result<(), Error> {
        for ((index, key), slot) in self.keys.iter().enumerate().zip(values) {
            let dtype = plain(self.batch.schema(), key.column)?.0;
            let array = self.batch.column(key.column);
            *slot = match crate::values::arrow_columns::decode_cell(array.as_ref(), dtype, row) {
                Some(value) => value,
                None => {
                    let column = match &mut self.decoded[index] {
                        Some(column) => column,
                        empty => empty.insert(key_column(self.batch, key.column)?),
                    };
                    column[row].clone()
                }
            };
        }
        Ok(())
    }
}

/// The first `fetch` rows of each group of `input` by `keys`, groups in key
/// order, ties in input order, as a full sort and a per-group limit give
/// them. Each group keeps a heap of at most `fetch` rows, read a batch at a
/// time, so it holds `groups × fetch` rows (plus displaced rows, until they
/// are compacted away) instead of its whole input. A row is tested against
/// its group's last kept row straight from the Arrow arrays; only a row
/// that is kept has its key values decoded.
async fn sort_with_fetch(
    mut input: Input<'_, Batch>,
    schema: &SchemaRef,
    keys: &[SortKey],
    groups: &[usize],
    fetch: u64,
    context: RunContext,
) -> Result<Batch, Error> {
    let mut table = super::group_table::GroupTable::new(groups.to_vec());
    let mut heaps: Vec<Vec<HeldRow>> = Vec::new();
    // The batches of held rows; row `r` of chunk `c` is `chunks[c]`'s row `r`.
    let mut chunks: Vec<Batch> = Vec::new();
    let mut workspace = Workspace::new(&context)?;
    let mut work = Cooperative::new(&context);
    let mut input_position = 0u64;
    let mut kept_rows = 0usize;
    let mut key_bytes = 0usize;
    let held_bytes = |chunks: &[Batch], key_bytes: usize, kept_rows: usize| {
        chunks.iter().map(Batch::memory_bytes).sum::<usize>()
            + key_bytes
            + kept_rows * std::mem::size_of::<HeldRow>()
    };
    let order = |a: &HeldRow, b: &HeldRow| {
        let key_order = keys
            .iter()
            .zip(a.keys.iter().zip(b.keys.iter()))
            .map(|(key, (a, b))| compare_held_key(a, b, key))
            .find(|order| order.is_ne())
            .unwrap_or(Ordering::Equal);
        sort_order(key_order, a.input_position, b.input_position)
    };
    let dtypes = keys
        .iter()
        .map(|key| plain(schema, key.column).map(|(dtype, _)| dtype.clone()))
        .collect::<Result<Vec<_>, _>>()?;
    let fetch = usize::try_from(fetch).unwrap_or(usize::MAX);
    // The groups the batch being read kept rows in, each once: a group is
    // added when its stamp is not yet that batch's number.
    let mut touched: Vec<u32> = Vec::new();
    let mut touched_in_batch: Vec<u64> = Vec::new();
    let mut batch_number = 0u64;
    while let Some(batch) = input.next().await {
        let batch = batch?;
        work.checkpoint_rows(batch.num_rows()).await?;
        if fetch == 0 {
            continue;
        }
        let mut kept_keys = KeptKeyValues::new(&batch, keys)?;
        batch_number += 1;
        let held = held_bytes(&chunks, key_bytes, kept_rows);
        let ids = chunked_group_ids(&mut table, &batch, &mut workspace, held, &mut work).await?;
        heaps.resize_with(table.len(), Vec::new);
        touched_in_batch.resize(table.len(), 0);
        touched.clear();
        // The chunk index this batch's kept rows will take.
        let reading = chunks.len() as u32;
        // Per held chunk, then the batch being read (last), the key
        // comparators of this batch's rows against its rows, built when a
        // heap's last row first lies there.
        let mut comparators: Vec<Option<Vec<arrow::array::DynComparator>>> =
            (0..=chunks.len()).map(|_| None).collect();
        // Whether each row of `batch` is still kept.
        let mut kept = vec![false; batch.num_rows()];
        for (row, &group) in ids.iter().enumerate() {
            let heap = &mut heaps[group as usize];
            let position = input_position + row as u64;
            let full = heap.len() >= fetch;
            if full {
                let last = &heap[0];
                let slot = last.chunk as usize;
                let compare = match &mut comparators[slot] {
                    Some(compare) => compare,
                    empty => {
                        let held = chunks.get(slot).unwrap_or(&batch);
                        empty.insert(
                            keys.iter()
                                .zip(&dtypes)
                                .map(|(key, dtype)| {
                                    key_comparator(
                                        batch.column(key.column),
                                        held.column(key.column),
                                        dtype,
                                        key,
                                    )
                                })
                                .collect::<Result<_, _>>()?,
                        )
                    }
                };
                let key_order = compare
                    .iter()
                    .map(|compare| compare(row, last.row as usize))
                    .find(|order| order.is_ne())
                    .unwrap_or(Ordering::Equal);
                if !sort_order(key_order, position, last.input_position).is_lt() {
                    continue;
                }
            }
            kept[row] = true;
            if touched_in_batch[group as usize] != batch_number {
                touched_in_batch[group as usize] = batch_number;
                touched.push(group);
            }
            if full {
                // The row displaces the group's last kept row: overwrite
                // that row in place, its key allocation reused.
                let last = &mut heap[0];
                key_bytes -= last.keys.iter().map(Value::bytes).sum::<usize>();
                if last.chunk == reading {
                    kept[last.row as usize] = false;
                }
                kept_keys.write(row, &mut last.keys)?;
                key_bytes += last.keys.iter().map(Value::bytes).sum::<usize>();
                (last.input_position, last.chunk, last.row) = (position, reading, row as u32);
                heap_sift_down_top(heap, &order);
            } else {
                let held = HeldRow {
                    keys: kept_keys.row(row)?,
                    input_position: position,
                    chunk: reading,
                    row: row as u32,
                };
                key_bytes += held.keys.iter().map(Value::bytes).sum::<usize>();
                heap_push(heap, held, &order);
                kept_rows += 1;
            }
        }
        input_position += batch.num_rows() as u64;
        // Take the batch's kept rows into chunk `reading`, and point their
        // held rows at their rows there.
        let mut chunk_rows = vec![0u32; batch.num_rows()];
        let mut taken = Vec::new();
        for (row, _) in kept.iter().enumerate().filter(|(_, &kept)| kept) {
            chunk_rows[row] = taken.len() as u32;
            taken.push(row as u32);
        }
        if !taken.is_empty() {
            for &group in &touched {
                for held in heaps[group as usize].iter_mut() {
                    if held.chunk == reading {
                        held.row = chunk_rows[held.row as usize];
                    }
                }
            }
            chunks.push(batch.take(&arrow::array::UInt32Array::from(taken))?);
        }
        let held_rows = chunks.iter().map(Batch::num_rows).sum::<usize>();
        if held_rows > 2 * kept_rows.max(1) {
            compact(schema, &mut chunks, &mut heaps)?;
        }
        workspace.set_bytes(
            table.memory_bytes()
                + touched_in_batch.len() * std::mem::size_of::<u64>()
                + held_bytes(&chunks, key_bytes, kept_rows),
        )?;
    }
    // Groups in key order, each group's rows in sort order, gathered from
    // the chunks in one copy.
    let mut rows = Vec::with_capacity(kept_rows);
    for group in table.groups_in_key_order() {
        if let Some(heap) = heaps.get_mut(group as usize) {
            heap.sort_unstable_by(order);
            rows.extend(
                heap.iter()
                    .map(|held| (held.chunk as usize, held.row as usize)),
            );
        }
    }
    Batch::interleave(schema, &chunks, &rows)
}

/// Gather the rows `heaps` keep into one chunk, in one copy, dropping
/// displaced rows, and point each kept row at its row there.
fn compact(
    schema: &SchemaRef,
    chunks: &mut Vec<Batch>,
    heaps: &mut [Vec<HeldRow>],
) -> Result<(), Error> {
    let mut rows = Vec::new();
    for held in heaps.iter_mut().flatten() {
        rows.push((held.chunk as usize, held.row as usize));
        held.chunk = 0;
        held.row = rows.len() as u32 - 1;
    }
    *chunks = vec![Batch::interleave(schema, chunks, &rows)?];
    Ok(())
}

/// [`compare_key`] for the key values of rows a sort with `fetch` holds,
/// with the common keys inline: two numbers, neither NaN (`-0` equal to
/// `0`), or two integers or times. NULL, NaN and every other type take
/// [`compare_key`]. The heap compares held rows about four times per kept
/// row; the full sort keeps [`compare_key`], which its merge runs faster
/// (inlining these arms there slowed a sort of 10^6 random numbers by a
/// sixth).
fn compare_held_key(a: &Value, b: &Value, key: &SortKey) -> Ordering {
    let order = match (a, b) {
        (Value::Float64(a), Value::Float64(b)) if !a.is_nan() && !b.is_nan() => {
            a.partial_cmp(b).expect("neither is NaN")
        }
        (Value::Int64(a), Value::Int64(b)) | (Value::Timestamp(a), Value::Timestamp(b)) => a.cmp(b),
        _ => return compare_key(a, b, key),
    };
    if key.descending {
        order.reverse()
    } else {
        order
    }
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
