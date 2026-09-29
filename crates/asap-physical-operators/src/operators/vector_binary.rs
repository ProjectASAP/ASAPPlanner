//! Label matching and scalar broadcasting are physical computation, not source binding.
use super::*;
use planner_types::{post_asap::BinaryOperator, pre_asap::BinaryOpKind};

pub(crate) fn value_schema(scalar: bool) -> Schema {
    let mut fields = Vec::new();
    if !scalar {
        fields.push(result_field(
            "labels",
            DataType::Map {
                key: Box::new(DataType::Utf8),
                value: Box::new(DataType::Utf8),
                value_nullable: false,
            },
            false,
        ));
    }
    fields.push(result_field(
        if scalar { "$promql_scalar" } else { "value" },
        DataType::Float64,
        false,
    ));
    schema(fields)
}

fn is_scalar(input: &Schema) -> Result<bool, Error> {
    for scalar in [true, false] {
        let expected = value_schema(scalar);
        if input.fields.len() == expected.fields.len()
            && input
                .fields
                .iter()
                .zip(&expected.fields)
                .all(|(a, b)| a.dtype == b.dtype && !a.nullable)
        {
            return Ok(scalar);
        }
    }
    Err(invalid(
        "vector binary requires Float64 scalars or complete label-map vectors",
    ))
}

impl Operator {
    pub fn vector_binary(
        left: Schema,
        right: Schema,
        operator: BinaryOperator,
        return_bool: bool,
    ) -> Result<Self, Error> {
        let scalar = is_scalar(&left)? && is_scalar(&right)?;
        is_scalar(&right)?;
        let expression = Expression::Binary {
            operator: operator.clone(),
            left: Box::new(Expression::Column(0)),
            right: Box::new(Expression::Column(1)),
        };
        expression.dtype(&schema(vec![
            result_field("left", DataType::Float64, false),
            result_field("right", DataType::Float64, false),
        ]))?;
        let comparison = matches!(operator.kind, BinaryOpKind::Compare(_));
        if (return_bool && !comparison) || (scalar && comparison && !return_bool) {
            return Err(invalid("invalid scalar/vector comparison bool mode"));
        }
        Ok(Self {
            inputs: vec![left, right],
            output: value_schema(scalar),
            kind: Kind::VectorBinary {
                operator,
                return_bool,
            },
        })
    }
}

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
