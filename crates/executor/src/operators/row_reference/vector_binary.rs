//! The row-at-a-time `VectorBinary` (reference for tests).
#![allow(clippy::type_complexity)]
use super::super::vector_binary::is_scalar;
use super::super::*;
use crate::expressions::binary::BinaryOpKind;

type Labels = BTreeMap<Arc<str>, Arc<str>>;
fn labels(row: &[Value]) -> Result<Labels, Error> {
    let Some(Value::Map(entries)) = row.first() else {
        return Err(invalid("vector requires label map"));
    };
    let mut result = BTreeMap::new();
    for (key, value) in entries.iter() {
        let (Value::Utf8(key), Value::Utf8(value)) = (key, value) else {
            return Err(invalid("labels must be Utf8"));
        };
        if result.insert(key.clone(), value.clone()).is_some() {
            return Err(invalid("duplicate label name"));
        }
    }
    Ok(result)
}
fn identity(mut labels: Labels) -> Labels {
    labels.remove("__name__");
    labels.retain(|_, value| !value.is_empty());
    labels
}
fn value(row: &[Value]) -> Result<f64, Error> {
    match row.last() {
        Some(Value::Float64(value)) => Ok(*value),
        _ => Err(invalid("binary value must be Float64")),
    }
}
fn label_bytes(labels: &Labels) -> usize {
    labels.iter().map(|(k, v)| 64 + k.len() + v.len()).sum()
}

pub(super) fn execute<'a>(
    op: &'a Operator,
    mut inputs: Vec<Input<'a, Batch>>,
    context: RunContext,
) -> Result<OutputStream<'a, Batch>, Error> {
    let Kind::VectorBinary {
        operator,
        return_bool,
    } = &op.kind
    else {
        unreachable!()
    };
    let left_scalar = is_scalar(&op.inputs[0])?;
    let right_scalar = is_scalar(&op.inputs[1])?;
    let right = inputs.pop().ok_or_else(|| invalid("missing right input"))?;
    let left = inputs.pop().ok_or_else(|| invalid("missing left input"))?;
    Ok(futures::stream::once(async move {
        let ((left, _left_memory), (right, _right_memory)) =
            futures::try_join!(collect_rows(left, &context), collect_rows(right, &context))?;
        if (left_scalar && left.len() != 1) || (right_scalar && right.len() != 1) {
            return Err(invalid("scalar input must contain exactly one value"));
        }
        let mut workspace = Workspace::new(&context)?;
        let mut work = Cooperative::new(&context);
        let mut rows = Vec::new();
        let mut result_identities = std::collections::BTreeSet::new();
        let mut emit = |labels: Labels, a: f64, b: f64| -> Result<(), Error> {
            let arithmetic = matches!(operator.kind, BinaryOpKind::Arithmetic(_));
            let result = match crate::expressions::arithmetic::evaluate_binary(operator, a, b)? {
                Value::Float64(value) => value,
                Value::Bool(value) if *return_bool => {
                    if value {
                        1.
                    } else {
                        0.
                    }
                }
                Value::Bool(true) => {
                    if left_scalar {
                        b
                    } else {
                        a
                    }
                }
                Value::Bool(false) => return Ok(()),
                _ => return Err(invalid("invalid binary result")),
            };
            let mut row = Vec::new();
            if !left_scalar || !right_scalar {
                let labels = if arithmetic || *return_bool {
                    identity(labels)
                } else {
                    labels
                };
                workspace.grow(label_bytes(&labels) + 64)?;
                if !result_identities.insert(labels.clone()) {
                    return Err(invalid("duplicate vector result labels"));
                }
                workspace.grow(
                    label_bytes(&labels)
                        + std::mem::size_of::<Vec<Value>>()
                        + 2 * std::mem::size_of::<Value>(),
                )?;
                row.push(Value::Map(
                    labels
                        .into_iter()
                        .map(|(k, v)| (Value::Utf8(k), Value::Utf8(v)))
                        .collect::<Vec<_>>()
                        .into(),
                ));
            } else {
                workspace.grow(std::mem::size_of::<Vec<Value>>() + std::mem::size_of::<Value>())?;
            }
            row.push(Value::Float64(result));
            rows.push(row);
            Ok(())
        };
        if left_scalar || right_scalar {
            let vectors = if left_scalar { &right } else { &left };
            for row in vectors {
                work.checkpoint().await?;
                let labels = if left_scalar && right_scalar {
                    Labels::new()
                } else {
                    labels(row)?
                };
                emit(
                    labels,
                    if left_scalar {
                        value(&left[0])?
                    } else {
                        value(row)?
                    },
                    if right_scalar {
                        value(&right[0])?
                    } else {
                        value(row)?
                    },
                )?;
            }
        } else {
            let mut rhs = BTreeMap::new();
            // Keep matching workspace separate from the output reservation captured by emit.
            let mut matching = Workspace::new(&context)?;
            for row in &right {
                work.checkpoint().await?;
                let key = identity(labels(row)?);
                matching.grow(label_bytes(&key) + 64)?;
                if rhs.insert(key, value(row)?).is_some() {
                    return Err(invalid("duplicate vector matching labels"));
                }
            }
            let mut seen = std::collections::BTreeSet::new();
            for row in &left {
                work.checkpoint().await?;
                let labels = labels(row)?;
                let key = identity(labels.clone());
                matching.grow(label_bytes(&key) + 64)?;
                if !seen.insert(key.clone()) {
                    return Err(invalid("duplicate vector matching labels"));
                }
                if let Some(b) = rhs.get(&key) {
                    emit(labels, value(row)?, *b)?;
                }
            }
        }
        Batch::try_new(op.output.clone(), rows)
    })
    .boxed_local())
}
