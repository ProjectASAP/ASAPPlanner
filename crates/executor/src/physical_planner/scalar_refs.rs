//! Scalar expressions that read another operator's output (PromQL
//! `scalar(v)`, SQL scalar subqueries, `EXISTS`, `IN`): each distinct
//! read binds its value as a column after the input's
//! ([`Operator::bind_scalar_ref`]), then the expression reads that column.
use super::lowering::{narrow, Lowering};
use super::*;
use crate::operators::ScalarRefRead;
use crate::values::plain;
use planner_types::ir::scalar::ScalarValue;

/// What one read takes of the operator it reads, as reads are told apart:
/// `IN` reads are distinct by their probe too. `a IN (S) OR b IN (S)` binds
/// two columns, `NOT EXISTS (S) OR EXISTS (S)` one.
#[derive(PartialEq)]
enum ReadForm {
    /// PromQL `scalar(v)` of `v`'s sample value column.
    VectorScalar {
        value: usize,
    },
    Scalar,
    Exists,
    In(ScalarExpr<PhysicalASAPNodeId>),
}

/// The node's own input and the operators its scalar expressions read:
/// `inputs` are the node's producers (`schemas` their outputs), the read
/// ones last, `scalar_ref_input_count` of them.
pub(super) struct ReadInputs<'a> {
    pub(super) inputs: &'a [NodeId],
    pub(super) schemas: &'a [SchemaRef],
    pub(super) scalar_ref_input_count: usize,
}

impl ReadInputs<'_> {
    /// The output schema of read operator `producer`.
    fn schema_of(&self, producer: PhysicalASAPNodeId) -> Result<SchemaRef, Error> {
        self.inputs
            .iter()
            .position(|&input| input == producer as NodeId)
            .map(|index| self.schemas[index].clone())
            .ok_or_else(|| invalid(format!("scalar read of node {producer} has no input edge")))
    }
}

/// `exprs` over `input` (node `input_id`) with each distinct read of
/// another operator bound as a column after `input`'s, one
/// [`Operator::bind_scalar_ref`] helper per read, chained: the last
/// helper's id and schema (`input`'s when nothing is read), and `exprs`
/// reading those columns. `m > scalar(sum(n))` binds `scalar(sum(n))` as
/// column `$read_0` and becomes `m > $read_0`.
pub(super) fn bind_reads(
    lowering: &mut Lowering<'_>,
    exprs: &[&ScalarExpr<PhysicalASAPNodeId>],
    input_id: NodeId,
    input: &SchemaRef,
    reads: &ReadInputs<'_>,
) -> Result<(NodeId, SchemaRef, Vec<ScalarExpr<PhysicalASAPNodeId>>), Error> {
    let width = input.fields.len();
    // The distinct reads, in order of first appearance.
    let mut distinct: Vec<(PhysicalASAPNodeId, ReadForm)> = Vec::new();
    let mut error = None;
    let mut bind = |expr: &ScalarExpr<PhysicalASAPNodeId>| -> ScalarExpr<PhysicalASAPNodeId> {
        let read = || -> Result<(PhysicalASAPNodeId, ReadForm, bool), Error> {
            Ok(match expr {
                ScalarExpr::PromqlScalarFromVector(producer) => {
                    // The vector's one numeric column is its sample value:
                    // Float64, or Int64 for `count`.
                    let schema = reads.schema_of(*producer)?;
                    let numeric: Vec<_> = (0..schema.fields.len())
                        .filter(|&i| {
                            plain(&schema, i).is_ok_and(|(dtype, _)| {
                                matches!(dtype, DataType::Float64 | DataType::Int64)
                            })
                        })
                        .collect();
                    let [value] = numeric[..] else {
                        return Err(invalid("scalar() reads a vector of one sample value"));
                    };
                    (*producer, ReadForm::VectorScalar { value }, false)
                }
                ScalarExpr::ScalarSubquery(producer) => (*producer, ReadForm::Scalar, false),
                ScalarExpr::Exists { subquery, negated } => (*subquery, ReadForm::Exists, *negated),
                ScalarExpr::InSubquery {
                    expr: probe,
                    subquery,
                    negated,
                } => (*subquery, ReadForm::In((**probe).clone()), *negated),
                _ => unreachable!("replace_operator_refs passes only operator reads"),
            })
        };
        let (producer, form, negated) = match read() {
            Ok(read) => read,
            Err(e) => {
                error.get_or_insert(e);
                return ScalarExpr::Literal(ScalarValue::Null);
            }
        };
        let index = match distinct
            .iter()
            .position(|(seen, seen_form)| *seen == producer && *seen_form == form)
        {
            Some(index) => index,
            None => {
                distinct.push((producer, form));
                distinct.len() - 1
            }
        };
        let column = ScalarExpr::Column(width + index);
        match negated {
            true => ScalarExpr::Not(Box::new(column)),
            false => column,
        }
    };
    let bound: Vec<_> = exprs
        .iter()
        .map(|expr| expr.replace_operator_refs(&mut bind))
        .collect();
    if let Some(error) = error {
        return Err(error);
    }
    let (mut current, mut current_schema) = (input_id, input.clone());
    for (index, (producer, form)) in distinct.into_iter().enumerate() {
        let read = match form {
            ReadForm::VectorScalar { value } => ScalarRefRead::VectorScalar { value },
            ReadForm::Scalar => ScalarRefRead::Scalar,
            ReadForm::Exists => ScalarRefRead::Exists,
            // The probe reads `input`'s columns, which keep their place in
            // the widened rows the read binds into.
            ReadForm::In(probe) => ScalarRefRead::In {
                probe: expression(&probe, &current_schema)?,
            },
        };
        let operator = Operator::bind_scalar_ref(
            current_schema,
            reads.schema_of(producer)?,
            read,
            &format!("$read_{index}"),
        )?;
        (current, current_schema) = lowering.helper(vec![current, producer as NodeId], operator)?;
    }
    Ok((current, current_schema, bound))
}

/// A Filter or Project whose expressions read other operators: their
/// reads bound ([`bind_reads`]), then the Filter or Project over the
/// widened rows (and, for a Filter, a projection back to the input's
/// columns).
pub(super) fn bind_scalar_refs(
    mut lowering: Lowering<'_>,
    operator: &NonASAPOp<PhysicalASAPNodeId>,
    reads: ReadInputs<'_>,
    output: &SchemaRef,
) -> Result<(), Error> {
    let own = reads.inputs.len() - reads.scalar_ref_input_count;
    let ([input_id], [input, ..]) = (&reads.inputs[..own], reads.schemas) else {
        return Err(invalid("a scalar read binds into one input"));
    };
    let exprs: Vec<_> = match operator {
        NonASAPOp::Filter { pred, .. } => vec![&pred.0],
        NonASAPOp::Project { cols, .. } => cols.iter().map(|col| &col.expr).collect(),
        _ => unreachable!("a Filter or a Project"),
    };
    let (current, current_schema, bound) =
        bind_reads(&mut lowering, &exprs, *input_id, input, &reads)?;
    match operator {
        NonASAPOp::Filter { .. } => {
            let filter = Operator::filter(
                current_schema.clone(),
                expression(&bound[0], &current_schema)?,
            )?;
            let (filtered, _) = lowering.helper(vec![current], filter)?;
            let narrowed = narrow(&current_schema, input.fields.len(), None, output)?;
            lowering.finish(vec![filtered], narrowed, output)
        }
        _ => {
            let project = Operator::project(
                current_schema.clone(),
                bound
                    .iter()
                    .zip(&output.fields)
                    .map(|(expr, field)| {
                        let expr = match expr {
                            ScalarExpr::Column(column) => Expression::Column(*column),
                            expr => expression(expr, &current_schema)?,
                        };
                        Ok((field.name.clone(), expr))
                    })
                    .collect::<Result<_, Error>>()?,
            )?;
            lowering.finish(vec![current], project, output)
        }
    }
}
