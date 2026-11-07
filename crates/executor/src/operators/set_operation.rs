//! SQL `INTERSECT` and `EXCEPT`, with or without `ALL`: rows compare as a
//! whole, NULL equal to NULL, as SQL set operations compare them.
use super::*;

impl Operator {
    /// The rows of `left` that `right` also has (`except` false) or lacks
    /// (`except` true). Without `all`, each such distinct row once; with
    /// `all`, a row `l` times in `left` and `r` times in `right` appears
    /// `min(l, r)` times in an intersection and `max(l - r, 0)` times in a
    /// difference. Both inputs have the plain columns of `left`'s types.
    pub fn set_operation(
        left: SchemaRef,
        right: SchemaRef,
        except: bool,
        all: bool,
    ) -> Result<Self, Error> {
        if left.fields.len() != right.fields.len()
            || (0..left.fields.len()).any(|i| match (plain(&left, i), plain(&right, i)) {
                (Ok((a, _)), Ok((b, _))) => a != b,
                _ => true,
            })
        {
            return Err(invalid("set operation inputs differ in their column types"));
        }
        Ok(Self {
            kind: Kind::SetOperation { except, all },
            inputs: vec![left.clone(), right],
            output: left,
        })
    }
}

pub(super) fn execute<'a>(
    operator: &'a Operator,
    mut inputs: Vec<Input<'a, Batch>>,
    context: RunContext,
) -> Result<OutputStream<'a, Batch>, Error> {
    let Kind::SetOperation { except, all } = operator.kind else {
        unreachable!()
    };
    let mut right = inputs.pop().ok_or_else(|| invalid("right input missing"))?;
    let mut left = inputs.pop().ok_or_else(|| invalid("left input missing"))?;
    Ok(futures::stream::once(async move {
        let columns: Vec<usize> = (0..operator.output.fields.len()).collect();
        let mut table = super::group_table::GroupTable::new(columns);
        let mut workspace = Workspace::new(&context)?;
        let mut work = Cooperative::new(&context);
        // The rows of each right group.
        let mut right_count = Vec::<u64>::new();
        while let Some(batch) = right.next().await {
            let batch = batch?;
            let held = right_count.capacity() * 8;
            let ids =
                chunked_group_ids(&mut table, &batch, &mut workspace, held, &mut work).await?;
            right_count.resize(table.len(), 0);
            for group in ids {
                right_count[group as usize] += 1;
            }
        }
        // Per left group, once first seen: how many of its rows to skip,
        // then how many to keep (`u64::MAX`: every one).
        let mut quota = Vec::<Option<(u64, u64)>>::new();
        let mut kept = Vec::new();
        while let Some(batch) = left.next().await {
            let batch = batch?;
            let held = right_count.capacity() * 8 + quota.capacity() * 24;
            let ids =
                chunked_group_ids(&mut table, &batch, &mut workspace, held, &mut work).await?;
            quota.resize(table.len(), None);
            let keep = ids
                .iter()
                .map(|&group| {
                    let (skip, keep) = quota[group as usize].get_or_insert_with(|| {
                        let in_right = right_count.get(group as usize).copied().unwrap_or(0);
                        match (except, all) {
                            (false, true) => (0, in_right),
                            (false, false) => (0, u64::from(in_right > 0)),
                            (true, true) => (in_right, u64::MAX),
                            (true, false) => (0, u64::from(in_right == 0)),
                        }
                    });
                    Some(if *skip > 0 {
                        *skip -= 1;
                        false
                    } else if *keep > 0 {
                        if *keep != u64::MAX {
                            *keep -= 1;
                        }
                        true
                    } else {
                        false
                    })
                })
                .collect::<arrow::array::BooleanArray>();
            let batch = batch.filter(&keep)?;
            workspace.grow(batch.memory_bytes())?;
            kept.push(batch);
        }
        Batch::concat(&operator.output, &kept)
    })
    .boxed_local())
}
