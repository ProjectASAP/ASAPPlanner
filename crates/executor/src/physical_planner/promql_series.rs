//! PromQL series nodes: `vector(s)`, which lowers to several native
//! operators, and the relabel value `label_replace` / `label_join` lower to.
use super::lowering::Lowering;
use super::scalar_refs::{bind_reads, ReadInputs};
use super::*;
use crate::operators::RelabelValue;
use planner_types::ir::scalar::ScalarValue;

/// PromQL `vector(s)`: one label-less sample of `s` at the evaluation time.
/// The evaluation time (one row), the operators `s` reads bound over it
/// ([`bind_reads`]), `s` projected as `value`, then the sample's
/// timestamp: `vector(1)` at 22:13:20 is `{} 1` stamped 22:13:20, and
/// `vector(scalar(n))` binds `scalar(n)` first.
pub(super) fn vector_from_scalar(
    mut lowering: Lowering<'_>,
    scalar: &ScalarExpr<PhysicalASAPNodeId>,
    reads: &ReadInputs<'_>,
    output: &SchemaRef,
) -> Result<(), Error> {
    let (time, time_schema) = lowering.helper(vec![], Operator::evaluation_time())?;
    let (bound, bound_schema, exprs) =
        bind_reads(&mut lowering, &[scalar], time, &time_schema, reads)?;
    let value = Operator::project(
        bound_schema.clone(),
        vec![("value".into(), expression(&exprs[0], &bound_schema)?)],
    )?;
    let (value, value_schema) = lowering.helper(vec![bound], value)?;
    lowering.finish(
        vec![value],
        Operator::scope_timestamp(value_schema, output.clone())?,
        output,
    )
}

/// What PromQL relabel `value` writes, from the expression the frontend
/// lowers `label_replace`, `label_join` or a constant label to:
/// `label_join(m, "dst", ",", "a", "b")` is `FunctionCall("label_join",
/// [",", Column(a), Column(b)])`, a [`RelabelValue::Join`].
pub(super) fn relabel_value(
    value: &ScalarExpr<PhysicalASAPNodeId>,
    input: &SchemaRef,
) -> Result<RelabelValue, Error> {
    let text = |expr: &ScalarExpr<PhysicalASAPNodeId>| match expr {
        ScalarExpr::Literal(ScalarValue::Utf8(text)) => Ok(text.clone()),
        _ => Err(invalid("a relabel argument is a text literal")),
    };
    let label = |expr: &ScalarExpr<PhysicalASAPNodeId>| match expr {
        ScalarExpr::Column(column) => input
            .fields
            .get(*column)
            .map(|field| field.name.clone())
            .ok_or_else(|| invalid("relabel source label out of range")),
        _ => Err(invalid("a relabel source is a label column")),
    };
    match value {
        ScalarExpr::Literal(_) => Ok(RelabelValue::Constant(text(value)?)),
        ScalarExpr::FunctionCall { name, args } => match (name.as_str(), args.as_slice()) {
            ("label_replace", [source, regex, replacement]) => Ok(RelabelValue::Replace {
                source: label(source)?,
                regex: text(regex)?,
                replacement: text(replacement)?,
            }),
            ("label_join", [separator, sources @ ..]) => Ok(RelabelValue::Join {
                separator: text(separator)?,
                sources: sources.iter().map(label).collect::<Result<_, _>>()?,
            }),
            _ => Err(invalid(format!(
                "relabel by {name} has no native implementation"
            ))),
        },
        _ => Err(invalid("relabel value has no native implementation")),
    }
}
