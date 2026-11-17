//! Planner nodes that lower to several native operators: helper operators
//! under helper ids ([`helper_id`]), then the node's own operator under the
//! node's id. A node one operator implements lowers in `bind_operation`
//! instead, so [`compile_node`] compiles it too.
//!
//! `SELECT x FROM t UNION ALL SELECT x FROM u` over branches whose columns
//! differ in nullability lowers to one projection per differing branch
//! (helpers 0, 1) and the union (the node's own operator).
use super::*;

/// What compiling one Planner node adds to the DAG: helper operators, then
/// the node's own.
pub(super) struct Lowering<'a> {
    pub(super) id: NodeId,
    pub(super) dag: &'a mut CompiledPhysicalDAG,
    /// The index ([`helper_id`]) the next helper takes.
    next_helper_index: u64,
}

impl Lowering<'_> {
    /// Add `operator` over `inputs` as the next helper: its id and schema.
    pub(super) fn helper(
        &mut self,
        inputs: Vec<NodeId>,
        operator: Operator,
    ) -> Result<(NodeId, SchemaRef), Error> {
        let id = helper_id(self.id, self.next_helper_index)?;
        self.next_helper_index += 1;
        let schema = operator.schema();
        self.dag.add(id, inputs, operator)?;
        Ok((id, schema))
    }

    /// Add the node's own `operator` over `inputs`, typed as `output`.
    pub(super) fn finish(
        self,
        inputs: Vec<NodeId>,
        operator: Operator,
        output: &SchemaRef,
    ) -> Result<(), Error> {
        self.dag.add(
            self.id,
            inputs,
            operator.with_output_schema(output.clone())?,
        )
    }
}

/// Compile `node` when it lowers to several operators, adding them to
/// `dag`: `Ok(false)` for any other node. `inputs` are the node's
/// producers (`schemas` their outputs), the ones its scalar expressions
/// read last; `scalar_ref_input_count` counts those.
pub(super) fn lower_to_several_operators(
    id: NodeId,
    node: &PhysicalASAPDAGNode,
    inputs: &[NodeId],
    schemas: &[SchemaRef],
    scalar_ref_input_count: usize,
    output: &SchemaRef,
    dag: &mut CompiledPhysicalDAG,
) -> Result<bool, Error> {
    let Payload::NonASAP(operator) = &node.payload else {
        return Ok(false);
    };
    let lowering = Lowering {
        id,
        dag,
        next_helper_index: 0,
    };
    let reads = super::scalar_refs::ReadInputs {
        inputs,
        schemas,
        scalar_ref_input_count,
    };
    match operator {
        NonASAPOp::Filter { .. } | NonASAPOp::Project { .. } if scalar_ref_input_count > 0 => {
            super::scalar_refs::bind_scalar_refs(lowering, operator, reads, output)?
        }
        NonASAPOp::PromqlVectorFromScalar(expr) => {
            super::promql_series::vector_from_scalar(lowering, expr, &reads, output)?
        }
        _ if scalar_ref_input_count > 0 => {
            return Err(invalid(
                "only a Filter, a Project or vector(s) binds a scalar read of another operator",
            ))
        }
        NonASAPOp::SetOp {
            kind: planner_types::ir::operator::RelationalSetOpKind::Union,
            all,
            ..
        } => super::relational::union(lowering, inputs, schemas, *all, output)?,
        NonASAPOp::Concat { .. } => {
            super::relational::union(lowering, inputs, schemas, true, output)?
        }
        NonASAPOp::SQLWindowFunc { .. } => {
            super::relational::sql_window(lowering, operator, inputs, schemas, output)?
        }
        NonASAPOp::Sort { keys, .. }
            if keys
                .iter()
                .any(|key| !matches!(key.expr, ScalarExpr::Column(_))) =>
        {
            super::relational::sort_by_expressions(lowering, operator, inputs, schemas, output)?
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// A projection of `input`'s columns, then `appended`: `count_values`'
/// value text, a sort key's value, a window function's argument.
pub(super) fn widening_projection(
    input: &SchemaRef,
    appended: impl IntoIterator<Item = (String, Expression)>,
) -> Result<Operator, Error> {
    let columns = input
        .fields
        .iter()
        .enumerate()
        .map(|(i, field)| (field.name.clone(), Expression::Column(i)))
        .chain(appended)
        .collect();
    Operator::project(input.clone(), columns)
}

/// `input` (node `input_id`) with `exprs`' values projected after its
/// columns, as the next helper: its id and schema.
pub(super) fn widen<'e>(
    lowering: &mut Lowering<'_>,
    input_id: NodeId,
    input: &SchemaRef,
    exprs: impl Iterator<Item = &'e ScalarExpr<PhysicalASAPNodeId>>,
) -> Result<(NodeId, SchemaRef), Error> {
    let appended = exprs
        .enumerate()
        .map(|(index, expr)| Ok((format!("$computed_{index}"), expression(expr, input)?)))
        .collect::<Result<Vec<_>, Error>>()?;
    lowering.helper(vec![input_id], widening_projection(input, appended)?)
}

/// A projection of `input`'s first `width` columns (then column `extra`, if
/// any) as `output`.
pub(super) fn narrow(
    input: &SchemaRef,
    width: usize,
    extra: Option<usize>,
    output: &SchemaRef,
) -> Result<Operator, Error> {
    let columns = (0..width)
        .chain(extra)
        .zip(&output.fields)
        .map(|(column, field)| (field.name.clone(), Expression::Column(column)))
        .collect();
    Operator::project(input.clone(), columns)?.with_output_schema(output.clone())
}

/// The native sort keys of the Planner's `keys`, each key's column given
/// by `column_of` (an error for a key it cannot place).
pub(super) fn sort_keys(
    keys: &[planner_types::ir::scalar::SortKey<PhysicalASAPNodeId>],
    mut column_of: impl FnMut(&ScalarExpr<PhysicalASAPNodeId>) -> Result<usize, Error>,
) -> Result<Vec<SortKey>, Error> {
    keys.iter()
        .map(|key| {
            Ok(SortKey {
                column: column_of(&key.expr)?,
                descending: !key.ascending,
                nulls_first: key.nulls_first,
            })
        })
        .collect()
}
