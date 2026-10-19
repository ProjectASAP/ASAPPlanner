//! Label matching and scalar broadcasting are physical computation, not source binding.
use super::*;
use crate::expressions::binary::BinaryOpKind;
use crate::expressions::binary::BinaryOperator;

pub(crate) fn value_schema(scalar: bool) -> SchemaRef {
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

pub(super) fn is_scalar(input: &SchemaRef) -> Result<bool, Error> {
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
        left: SchemaRef,
        right: SchemaRef,
        operator: BinaryOperator,
        mut return_bool: bool,
    ) -> Result<Self, Error> {
        let mut operator = operator;
        // The IR's `bool` comparison is this operator's `return_bool` mode.
        if let BinaryOpKind::CompareBool(op) = &operator.kind {
            operator.kind = BinaryOpKind::Compare(op.clone());
            return_bool = true;
        }
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
/// A vector row's label set, from its label map.
fn labels(map: &Value) -> Result<Labels, Error> {
    let Value::Map(entries) = map else {
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
fn label_bytes(labels: &Labels) -> usize {
    labels.iter().map(|(k, v)| 64 + k.len() + v.len()).sum()
}

/// One operand's columns: label maps (none for a scalar) and values.
struct Operand {
    labels: Vec<Value>,
    values: Vec<f64>,
}

fn operand(batch: &Batch, scalar: bool) -> Result<Operand, Error> {
    let value_column = batch.schema().fields.len() - 1;
    let values = decoded(batch, value_column)?
        .into_iter()
        .map(|value| match value {
            Value::Float64(value) => Ok(value),
            _ => Err(invalid("binary value must be Float64")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let labels = if scalar {
        Vec::new()
    } else {
        decoded(batch, 0)?
    };
    Ok(Operand { labels, values })
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
        let (left_input, right_input) = futures::try_join!(
            collect_batch(left, &op.inputs[0], &context),
            collect_batch(right, &op.inputs[1], &context)
        )?;
        let left = operand(&left_input.batch, left_scalar)?;
        let right = operand(&right_input.batch, right_scalar)?;
        if (left_scalar && left.values.len() != 1) || (right_scalar && right.values.len() != 1) {
            return Err(invalid("scalar input must contain exactly one value"));
        }
        let mut workspace = Workspace::new(&context)?;
        let mut work = Cooperative::new(&context);
        // The output columns: label maps (vectors only) and values.
        let mut out_labels: Vec<Value> = Vec::new();
        let mut out_values: Vec<f64> = Vec::new();
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
                workspace.grow(label_bytes(&labels) + 16)?;
                out_labels.push(Value::Map(
                    labels
                        .into_iter()
                        .map(|(k, v)| (Value::Utf8(k), Value::Utf8(v)))
                        .collect::<Vec<_>>()
                        .into(),
                ));
            } else {
                workspace.grow(8)?;
            }
            out_values.push(result);
            Ok(())
        };
        if left_scalar || right_scalar {
            let vector = if left_scalar { &right } else { &left };
            for row in 0..vector.values.len() {
                let labels = if left_scalar && right_scalar {
                    Labels::new()
                } else {
                    labels(&vector.labels[row])?
                };
                emit(
                    labels,
                    if left_scalar {
                        left.values[0]
                    } else {
                        vector.values[row]
                    },
                    if right_scalar {
                        right.values[0]
                    } else {
                        vector.values[row]
                    },
                )?;
            }
            work.checkpoint_rows(vector.values.len()).await?;
        } else {
            let mut rhs = BTreeMap::new();
            // Keep matching workspace separate from the output reservation captured by emit.
            let mut matching = Workspace::new(&context)?;
            for row in 0..right.values.len() {
                let key = identity(labels(&right.labels[row])?);
                matching.grow(label_bytes(&key) + 64)?;
                if rhs.insert(key, right.values[row]).is_some() {
                    return Err(invalid("duplicate vector matching labels"));
                }
            }
            work.checkpoint_rows(right.values.len()).await?;
            let mut seen = std::collections::BTreeSet::new();
            for row in 0..left.values.len() {
                let labels = labels(&left.labels[row])?;
                let key = identity(labels.clone());
                matching.grow(label_bytes(&key) + 64)?;
                if !seen.insert(key.clone()) {
                    return Err(invalid("duplicate vector matching labels"));
                }
                if let Some(b) = rhs.get(&key) {
                    emit(labels, left.values[row], *b)?;
                }
            }
            work.checkpoint_rows(left.values.len()).await?;
        }
        let rows = out_values.len();
        let mut arrays = Vec::new();
        if !left_scalar || !right_scalar {
            arrays.push(crate::values::arrow_columns::encode(
                op.output.fields[0].plain_dtype().expect("plain"),
                crate::values::arrow_columns::ColumnDepth::BatchColumn,
                &out_labels.iter().collect::<Vec<_>>(),
            )?);
        }
        arrays
            .push(Arc::new(arrow::array::Float64Array::from(out_values)) as arrow::array::ArrayRef);
        Batch::from_parts(
            op.output.clone(),
            arrays,
            vec![None; op.output.fields.len()],
            rows,
        )
    })
    .boxed_local())
}
