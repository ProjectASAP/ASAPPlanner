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

/// The key columns of `batch` (in `schema`) as a batch of their own, so
/// both sides of a match share one group table.
pub(super) fn key_batch(batch: &Batch, columns: &[usize]) -> Result<Batch, Error> {
    let fields = columns
        .iter()
        .map(|&c| batch.schema().fields[c].clone())
        .collect();
    batch.project(schema(fields), columns)
}

/// Whether row `row` of a key batch can match: no NULL and no NaN key.
pub(super) fn matchable_rows(keys: &Batch) -> Vec<bool> {
    let mut matchable = vec![true; keys.num_rows()];
    for column in keys.record_batch().columns() {
        let nulls = column.logical_nulls();
        let floats = column.as_any().downcast_ref::<arrow::array::Float64Array>();
        for (row, slot) in matchable.iter_mut().enumerate() {
            if nulls.as_ref().is_some_and(|nulls| nulls.is_null(row))
                || floats.is_some_and(|floats| floats.value(row).is_nan())
            {
                *slot = false;
            }
        }
    }
    matchable
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
        let (left, right) = futures::try_join!(
            collect_batch(left, &op.inputs[0], &context),
            collect_batch(right, &op.inputs[1], &context)
        )?;
        let (left, right) = (&left.batch, &right.batch);
        if left.num_rows() == 0 || left.num_rows() != right.num_rows() {
            return Err(invalid(
                "aligned arithmetic requires matching nonempty key sets",
            ));
        }
        let mut work = Cooperative::new(&context);
        let mut workspace = Workspace::new(&context)?;
        let left_keys = key_batch(left, &keys.iter().map(|&(l, _)| l).collect::<Vec<_>>())?;
        let right_keys = key_batch(right, &keys.iter().map(|&(_, r)| r).collect::<Vec<_>>())?;
        let mut table = super::group_table::GroupTable::new((0..keys.len()).collect());
        let float_values = |batch: &Batch, column: usize| {
            batch
                .column(column)
                .as_any()
                .downcast_ref::<arrow::array::Float64Array>()
                .cloned()
                .ok_or_else(|| invalid("invalid aligned value"))
        };
        // The right side's value of each key.
        let right_values = float_values(right, values.1)?;
        let mut indexed: Vec<Option<f64>> = Vec::new();
        for (row, group) in table.group_ids(&right_keys)?.into_iter().enumerate() {
            let value = right_values.value(row);
            if !value.is_finite() {
                return Err(invalid("aligned arithmetic input is non-finite"));
            }
            indexed.resize(table.len(), None);
            if indexed[group as usize].replace(value).is_some() {
                return Err(invalid("aligned arithmetic input has duplicate keys"));
            }
        }
        work.checkpoint_rows(right.num_rows()).await?;
        workspace.set_bytes(table.memory_bytes() + 16 * indexed.len())?;
        let left_values = float_values(left, values.0)?;
        let mut results = Vec::with_capacity(left.num_rows());
        for (row, group) in table.group_ids(&left_keys)?.into_iter().enumerate() {
            let right = indexed
                .get_mut(group as usize)
                .and_then(Option::take)
                .ok_or_else(|| invalid("aligned arithmetic input has missing or duplicate keys"))?;
            let left = left_values.value(row);
            if !left.is_finite() {
                return Err(invalid("aligned arithmetic input is non-finite"));
            }
            let result = crate::expressions::arithmetic::evaluate_binary(operator, left, right)?;
            let Value::Float64(result) = result else {
                return Err(invalid("aligned arithmetic produced a non-finite update"));
            };
            if !result.is_finite() {
                return Err(invalid("aligned arithmetic produced a non-finite update"));
            }
            results.push(result);
        }
        work.checkpoint_rows(left.num_rows()).await?;
        if indexed.iter().any(Option::is_some) {
            return Err(invalid("aligned arithmetic has unmatched input keys"));
        }
        let output = left.with_plain_column(
            values.0,
            Arc::new(arrow::array::Float64Array::from(results)),
        )?;
        workspace.grow(output.memory_bytes())?;
        Ok(output)
    })
    .boxed_local())
}
