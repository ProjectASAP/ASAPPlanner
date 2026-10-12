//! Arithmetic on complete, aligned population/window rows used by precomputation.
use super::*;
use crate::expressions::binary::BinaryOpKind;
use crate::expressions::binary::BinaryOperator;

use std::collections::BTreeSet;

impl Operator {
    /// Match every row by the declared identity columns. Unlike an inner join,
    /// incomplete or duplicate keys are errors: dropping an update changes state.
    pub fn aligned_binary(
        left: SchemaRef,
        right: SchemaRef,
        keys: Vec<(usize, usize)>,
        values: (usize, usize),
        operator: BinaryOperator,
    ) -> Result<Self, Error> {
        if keys.is_empty()
            || !matches!(operator.kind, BinaryOpKind::Arithmetic(_))
            || operator.vector_match.is_some()
        {
            return Err(invalid(
                "aligned arithmetic requires explicit keys and arithmetic semantics",
            ));
        }
        for (input, value) in [(&left, values.0), (&right, values.1)] {
            if input.fields.get(value).is_none_or(|f| {
                f.nullable || f.dtype != SummaryFamilyType::Plain(DataType::Float64)
            }) {
                return Err(invalid(
                    "aligned arithmetic requires non-null Float64 values",
                ));
            }
        }
        let mut left_keys = BTreeSet::new();
        let mut right_keys = BTreeSet::new();
        for &(l, r) in &keys {
            if l == values.0
                || r == values.1
                || !left_keys.insert(l)
                || !right_keys.insert(r)
                || left
                    .fields
                    .get(l)
                    .zip(right.fields.get(r))
                    .is_none_or(|(l, r)| l.nullable || r.nullable || l.dtype != r.dtype)
            {
                return Err(invalid("invalid aligned arithmetic keys"));
            }
        }
        if left_keys.len() + 1 != left.fields.len() || right_keys.len() + 1 != right.fields.len() {
            return Err(invalid(
                "aligned arithmetic must account for every input column",
            ));
        }
        Ok(Self {
            output: left.clone(),
            inputs: vec![left, right],
            kind: Kind::AlignedBinary {
                keys,
                values,
                operator,
            },
        })
    }
}

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
