//! The row-at-a-time `AlignedBinary` (reference for tests).
use super::super::*;

pub(super) fn execute<'a>(
    op: &'a Operator,
    mut inputs: Vec<Input<'a, Batch>>,
    context: RunContext,
) -> Result<OutputStream<'a, Batch>, Error> {
    let Kind::AlignedBinary {
        keys,
        values,
        operator,
    } = &op.kind
    else {
        unreachable!()
    };
    let right = inputs
        .pop()
        .ok_or_else(|| invalid("missing aligned right input"))?;
    let left = inputs
        .pop()
        .ok_or_else(|| invalid("missing aligned left input"))?;
    Ok(futures::stream::once(async move {
        let ((left, _left_memory), (right, _right_memory)) =
            futures::try_join!(collect_rows(left, &context), collect_rows(right, &context))?;
        if left.is_empty() || left.len() != right.len() {
            return Err(invalid(
                "aligned arithmetic requires matching nonempty key sets",
            ));
        }
        let mut work = Cooperative::new(&context);
        let mut workspace = Workspace::new(&context)?;
        let columns = |side: bool| {
            keys.iter()
                .map(|&(l, r)| if side { r } else { l })
                .collect::<Vec<_>>()
        };
        let left_columns = columns(false);
        let right_columns = columns(true);
        let mut indexed = BTreeMap::new();
        for row in right {
            work.checkpoint().await?;
            let key = group_key(&row, &right_columns)?;
            let Value::Float64(value) = row[values.1] else {
                return Err(invalid("invalid aligned value"));
            };
            if !value.is_finite() {
                return Err(invalid("aligned arithmetic input is non-finite"));
            }
            workspace.grow(key_bytes(&key) + 64)?;
            if indexed.insert(key, value).is_some() {
                return Err(invalid("aligned arithmetic input has duplicate keys"));
            }
        }
        let mut rows = Vec::new();
        for mut row in left {
            work.checkpoint().await?;
            let key = group_key(&row, &left_columns)?;
            let right = indexed
                .remove(&key)
                .ok_or_else(|| invalid("aligned arithmetic input has missing or duplicate keys"))?;
            let Value::Float64(left) = row[values.0] else {
                return Err(invalid("invalid aligned value"));
            };
            if !left.is_finite() {
                return Err(invalid("aligned arithmetic input is non-finite"));
            }
            let result = crate::expressions::arithmetic::evaluate_binary(operator, left, right)?;
            if !matches!(result, Value::Float64(value) if value.is_finite()) {
                return Err(invalid("aligned arithmetic produced a non-finite update"));
            }
            row[values.0] = result;
            workspace.grow(std::mem::size_of::<Vec<Value>>())?;
            rows.push(row);
        }
        if !indexed.is_empty() {
            return Err(invalid("aligned arithmetic has unmatched input keys"));
        }
        Batch::try_new(op.output.clone(), rows)
    })
    .boxed_local())
}
