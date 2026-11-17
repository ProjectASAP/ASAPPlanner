//! SQL relational nodes that lower to several native operators: `UNION` /
//! `Concat` over branches conformed to one schema, SQL window functions
//! over computed arguments and order keys, and `Sort` by expressions.
use super::lowering::{narrow, sort_keys, widen, Lowering};
use super::*;
use crate::operators::SqlWindowSpec;
use crate::values::{plain, Value};
use planner_types::ir::operator::WindowFuncKind;

/// `UNION ALL` (`all`) or `UNION` of `inputs`, in order: each branch
/// projected to `output` when its schema differs, then the union; `UNION`
/// keeps each distinct row once. The SQL frontend lowers `UNION` to a
/// `Dedup` over a `UNION ALL`, so only an IR built directly reaches the
/// distinct form.
pub(super) fn union(
    mut lowering: Lowering<'_>,
    inputs: &[NodeId],
    schemas: &[SchemaRef],
    all: bool,
    output: &SchemaRef,
) -> Result<(), Error> {
    let branches = inputs
        .iter()
        .zip(schemas)
        .map(|(&input, schema)| {
            if schema == output {
                return Ok(input);
            }
            if schema.fields.len() != output.fields.len() {
                return Err(invalid("union branches differ in width"));
            }
            let conformed = narrow(schema, schema.fields.len(), None, output)?;
            Ok(lowering.helper(vec![input], conformed)?.0)
        })
        .collect::<Result<Vec<_>, Error>>()?;
    let union = Operator::union(output.clone(), branches.len())?;
    if all {
        return lowering.finish(branches, union, output);
    }
    let (union, _) = lowering.helper(branches, union)?;
    let all_columns = (0..output.fields.len()).collect();
    let distinct = Operator::limit(output.clone(), 1, 0, all_columns)?;
    lowering.finish(vec![union], distinct, output)
}

/// A `Sort` with an expression key: the keys' values projected after the
/// input's columns, the sort by them, then a projection back to the
/// input's columns. `ORDER BY v * 10 + b DESC` sorts by the projected
/// column `$computed_0`.
pub(super) fn sort_by_expressions(
    mut lowering: Lowering<'_>,
    operator: &NonASAPOp<PhysicalASAPNodeId>,
    inputs: &[NodeId],
    schemas: &[SchemaRef],
    output: &SchemaRef,
) -> Result<(), Error> {
    let (
        NonASAPOp::Sort {
            keys, partition_by, ..
        },
        [input_id],
        [input],
    ) = (operator, inputs, schemas)
    else {
        return Err(invalid("Sort reads one input"));
    };
    let width = input.fields.len();
    let (widened, widened_schema) = widen(
        &mut lowering,
        *input_id,
        input,
        keys.iter().map(|key| &key.expr),
    )?;
    let mut next = width;
    let sort = Operator::sort(
        widened_schema.clone(),
        sort_keys(keys, |_| {
            next += 1;
            Ok(next - 1)
        })?,
        groups(input, partition_by)?,
        None,
    )?;
    let (sorted, _) = lowering.helper(vec![widened], sort)?;
    let narrowed = narrow(&widened_schema, width, None, output)?;
    lowering.finish(vec![sorted], narrowed, output)
}

/// SQL window function `operator` over its one input: its argument and
/// order keys projected first when they are expressions, then the window
/// over its partitions, then a projection dropping the computed columns.
pub(super) fn sql_window(
    mut lowering: Lowering<'_>,
    operator: &NonASAPOp<PhysicalASAPNodeId>,
    inputs: &[NodeId],
    schemas: &[SchemaRef],
    output: &SchemaRef,
) -> Result<(), Error> {
    let (
        NonASAPOp::SQLWindowFunc {
            func: function,
            args,
            partition_by,
            order_by,
            frame,
            output_name,
            ..
        },
        [input_id],
        [input],
    ) = (operator, inputs, schemas)
    else {
        return Err(invalid("a window function reads one input"));
    };
    let width = input.fields.len();
    // The value argument and the order keys: a column of the input, or an
    // expression projected after its columns.
    let value = args.first().filter(|_| {
        !matches!(
            function,
            WindowFuncKind::RowNumber | WindowFuncKind::Rank | WindowFuncKind::DenseRank
        )
    });
    let computed: Vec<_> = value
        .into_iter()
        .chain(order_by.iter().map(|key| &key.expr))
        .filter(|expr| !matches!(expr, ScalarExpr::Column(_)))
        .collect();
    let mut next = width;
    let mut column_of = |expr: &ScalarExpr<PhysicalASAPNodeId>| match expr {
        ScalarExpr::Column(column) => *column,
        _ => {
            next += 1;
            next - 1
        }
    };
    let argument = value.map(&mut column_of);
    let order = sort_keys(order_by, |expr| Ok(column_of(expr)))?;
    let (window_input, window_schema) = match computed.is_empty() {
        true => (*input_id, input.clone()),
        false => widen(&mut lowering, *input_id, input, computed.into_iter())?,
    };
    // `LAG` / `LEAD` (…InFrame)'s literal offset and default value, the
    // default cast to the value's type as DataFusion coerces it.
    let literal = |index: usize, to: Option<&DataType>| -> Result<Option<Value>, Error> {
        let Some(expr) = args.get(index) else {
            return Ok(None);
        };
        let expr = match to {
            Some(to) => ScalarExpr::Cast {
                expr: Box::new(expr.clone()),
                to: to.clone(),
                try_cast: false,
            },
            None => expr.clone(),
        };
        let empty = Arc::new(planner_types::ir::schema::Schema::default());
        expression(&expr, &empty)?.evaluate(&[]).map(Some)
    };
    let offsetting = matches!(
        function,
        WindowFuncKind::Lag
            | WindowFuncKind::Lead
            | WindowFuncKind::LagInFrame
            | WindowFuncKind::LeadInFrame
    );
    let (offset, default) = match offsetting {
        false => (1, Value::Null),
        true => {
            let value_type = argument
                .map(|column| plain(&window_schema, column).map(|(dtype, _)| dtype.clone()))
                .transpose()?;
            let offset = match literal(1, None)? {
                None => 1,
                Some(Value::Int64(offset)) => offset,
                Some(_) => return Err(invalid("a LAG / LEAD offset is an integer literal")),
            };
            (
                offset,
                literal(2, value_type.as_ref())?.unwrap_or(Value::Null),
            )
        }
    };
    let window = Operator::sql_window(
        window_schema.clone(),
        SqlWindowSpec {
            function: function.clone(),
            argument,
            offset,
            default,
            partition: groups(input, partition_by)?,
            order,
            frame: frame.clone(),
        },
        output_name,
    )?;
    if window_schema.fields.len() == width {
        return lowering.finish(vec![window_input], window, output);
    }
    let windowed_schema = window.schema();
    let (windowed, _) = lowering.helper(vec![window_input], window)?;
    let narrowed = narrow(
        &windowed_schema,
        width,
        Some(windowed_schema.fields.len() - 1),
        output,
    )?;
    lowering.finish(vec![windowed], narrowed, output)
}
