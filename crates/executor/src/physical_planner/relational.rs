//! Lowering of the relational operators (`Dedup`, `SetOp`, `Concat`, SQL
//! window functions, `Sort` by expressions), the PromQL series operators
//! (`vector(s)`, `label_replace` / `label_join`, `limitk` /
//! `limit_ratio`), and scalar expressions that read another operator, each
//! to native operators and the helpers they need.
use super::*;
use crate::operators::{RelabelValue, SubqueryRead, WindowSpec};
use crate::values::{plain, Value};
use planner_types::ir::operator::{RelationalSetOpKind, WindowFuncKind};
use planner_types::ir::scalar::ScalarValue;

/// What compiling one Planner node adds to the DAG: operators under
/// helper ids, then the node's own.
struct Lowering<'a> {
    id: NodeId,
    dag: &'a mut CompiledPhysicalDAG,
    helpers: u64,
}

impl Lowering<'_> {
    /// Add `operator` over `inputs` as the next helper: its id and schema.
    fn helper(
        &mut self,
        inputs: Vec<NodeId>,
        operator: Operator,
    ) -> Result<(NodeId, SchemaRef), Error> {
        let id = helper_id(self.id, self.helpers)?;
        self.helpers += 1;
        let schema = operator.schema();
        self.dag.add(id, inputs, operator)?;
        Ok((id, schema))
    }
}

/// Compile `node` when this module owns its lowering, adding it (and its
/// helpers) to `dag`: `Ok(false)` for any other node. `inputs` are the
/// node's producers (`schemas` their outputs), its scalar reads last;
/// `scalar_reads` counts those.
pub(super) fn lower(
    id: NodeId,
    node: &PhysicalASAPDAGNode,
    inputs: &[NodeId],
    schemas: &[SchemaRef],
    scalar_reads: usize,
    output: &SchemaRef,
    dag: &mut CompiledPhysicalDAG,
) -> Result<bool, Error> {
    let Payload::NonASAP(operator) = &node.payload else {
        return Ok(false);
    };
    let mut lowering = Lowering {
        id,
        dag,
        helpers: 0,
    };
    let operator =
        match operator {
            NonASAPOp::Filter { .. } | NonASAPOp::Project { .. } if scalar_reads > 0 => {
                return bind_scalar_reads(
                    &mut lowering,
                    operator,
                    inputs,
                    schemas,
                    scalar_reads,
                    output,
                )
                .map(|()| true);
            }
            _ if scalar_reads > 0 => {
                return Err(invalid(
                    "only a Filter or a Project binds a scalar read of another operator",
                ))
            }
            // SQL `DISTINCT`: the first row of each distinct value of `cols`.
            NonASAPOp::Dedup { cols, .. } => {
                let [input] = schemas else {
                    return Err(invalid("Dedup reads one input"));
                };
                let columns = match cols.is_empty() {
                    true => (0..input.fields.len()).collect(),
                    false => cols.clone(),
                };
                Operator::limit(input.clone(), 1, 0, columns)?
            }
            NonASAPOp::SetOp { kind, all, .. } => {
                let [left, right] = schemas else {
                    return Err(invalid("a set operation reads two inputs"));
                };
                if *kind != RelationalSetOpKind::Union {
                    Operator::set_operation(
                        left.clone(),
                        right.clone(),
                        *kind == RelationalSetOpKind::Except,
                        *all,
                    )?
                } else {
                    let branches = conformed_inputs(&mut lowering, inputs, schemas, output)?;
                    let union = Operator::union(output.clone(), branches.len())?;
                    if *all {
                        lowering.dag.add(id, branches, union)?;
                    } else {
                        // `UNION` keeps each distinct row once.
                        let (union, _) = lowering.helper(branches, union)?;
                        let all_columns = (0..output.fields.len()).collect();
                        let distinct = Operator::limit(output.clone(), 1, 0, all_columns)?;
                        lowering.dag.add(id, vec![union], distinct)?;
                    }
                    return Ok(true);
                }
            }
            // `UNION ALL` of its branches, in order.
            NonASAPOp::Concat { .. } => {
                let branches = conformed_inputs(&mut lowering, inputs, schemas, output)?;
                let union = Operator::union(output.clone(), branches.len())?;
                lowering.dag.add(id, branches, union)?;
                return Ok(true);
            }
            NonASAPOp::SQLWindowFunc { .. } => {
                let [input] = schemas else {
                    return Err(invalid("a window function reads one input"));
                };
                return sql_window(&mut lowering, inputs, input, output, operator).map(|()| true);
            }
            NonASAPOp::Sort {
                keys, partition_by, ..
            } if keys
                .iter()
                .any(|key| !matches!(key.expr, ScalarExpr::Column(_))) =>
            {
                let [input] = schemas else {
                    return Err(invalid("Sort reads one input"));
                };
                // Sort by the keys' values, projected after the input's columns.
                let width = input.fields.len();
                let (widened, widened_schema) = widen(
                    &mut lowering,
                    inputs,
                    input,
                    keys.iter().map(|key| &key.expr),
                )?;
                let sort = Operator::sort(
                    widened_schema.clone(),
                    keys.iter()
                        .enumerate()
                        .map(|(index, key)| crate::operators::SortKey {
                            column: width + index,
                            descending: !key.ascending,
                            nulls_first: key.nulls_first,
                        })
                        .collect(),
                    groups(input, partition_by)?,
                )?;
                let (sorted, _) = lowering.helper(vec![widened], sort)?;
                let narrowed = narrow(&widened_schema, width, None, output)?;
                lowering.dag.add(id, vec![sorted], narrowed)?;
                return Ok(true);
            }
            // PromQL `vector(s)`: one label-less sample of `s` at the
            // evaluation time.
            NonASAPOp::PromqlVectorFromScalar(expr) => {
                let (time, time_schema) = lowering.helper(vec![], Operator::evaluation_time())?;
                let value = Operator::project(
                    time_schema.clone(),
                    vec![("value".into(), expression(expr, &time_schema)?)],
                )?;
                let (value, value_schema) = lowering.helper(vec![time], value)?;
                lowering.dag.add(
                    id,
                    vec![value],
                    Operator::scope_timestamp(value_schema, output.clone())?,
                )?;
                return Ok(true);
            }
            NonASAPOp::PromqlRelabel { dst, value, .. } => {
                let [input] = schemas else {
                    return Err(invalid("a relabel reads one input"));
                };
                Operator::series_relabel(
                    input.clone(),
                    output.clone(),
                    dst.clone(),
                    relabel_value(value, input)?,
                )?
            }
            NonASAPOp::PromqlSeriesSample { by, kind, .. } => {
                let [input] = schemas else {
                    return Err(invalid("limitk / limit_ratio reads one input"));
                };
                Operator::series_sample(input.clone(), groups(input, by)?, *kind)?
            }
            NonASAPOp::PromqlInfoEnrich { .. } => return Err(invalid(
                "info() joins its info metric, which the plan does not carry as an input (#1244)",
            )),
            _ => return Ok(false),
        };
    lowering.dag.add(
        id,
        inputs.to_vec(),
        operator.with_output_schema(output.clone())?,
    )?;
    Ok(true)
}

/// `inputs`, each projected to `output` (its names and nullability) when
/// its schema differs, as a union reads its branches.
fn conformed_inputs(
    lowering: &mut Lowering<'_>,
    inputs: &[NodeId],
    schemas: &[SchemaRef],
    output: &SchemaRef,
) -> Result<Vec<NodeId>, Error> {
    inputs
        .iter()
        .zip(schemas)
        .map(|(&input, schema)| {
            if schema == output {
                return Ok(input);
            }
            if schema.fields.len() != output.fields.len() {
                return Err(invalid("union branches differ in width"));
            }
            let narrowed = narrow(schema, schema.fields.len(), None, output)?;
            Ok(lowering.helper(vec![input], narrowed)?.0)
        })
        .collect()
}

/// A projection of `input`'s first `width` columns (then column `extra`, if
/// any) as `output`.
fn narrow(
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

/// `input` (the node's one producer, `inputs[0]`) with `exprs`' values
/// projected after its columns: the projection's id and schema.
fn widen<'e>(
    lowering: &mut Lowering<'_>,
    inputs: &[NodeId],
    input: &SchemaRef,
    exprs: impl Iterator<Item = &'e ScalarExpr<PhysicalASAPNodeId>>,
) -> Result<(NodeId, SchemaRef), Error> {
    let mut columns = input
        .fields
        .iter()
        .enumerate()
        .map(|(i, field)| (field.name.clone(), Expression::Column(i)))
        .collect::<Vec<_>>();
    for (index, expr) in exprs.enumerate() {
        columns.push((format!("$computed_{index}"), expression(expr, input)?));
    }
    lowering.helper(
        inputs[..1].to_vec(),
        Operator::project(input.clone(), columns)?,
    )
}

/// SQL window function `operator` over `input`: its argument and order
/// keys projected first when they are expressions, then the window over
/// its partitions.
fn sql_window(
    lowering: &mut Lowering<'_>,
    inputs: &[NodeId],
    input: &SchemaRef,
    output: &SchemaRef,
    operator: &NonASAPOp<PhysicalASAPNodeId>,
) -> Result<(), Error> {
    let NonASAPOp::SQLWindowFunc {
        func: function,
        args,
        partition_by,
        order_by,
        frame,
        output_name,
        ..
    } = operator
    else {
        unreachable!("a window function")
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
    let order = order_by
        .iter()
        .map(|key| crate::operators::SortKey {
            column: column_of(&key.expr),
            descending: !key.ascending,
            nulls_first: key.nulls_first,
        })
        .collect();
    let (window_input, window_schema) = match computed.is_empty() {
        true => (inputs[0], input.clone()),
        false => widen(lowering, inputs, input, computed.into_iter())?,
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
        WindowSpec {
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
        lowering.dag.add(
            lowering.id,
            vec![window_input],
            window.with_output_schema(output.clone())?,
        )?;
        return Ok(());
    }
    let windowed_schema = window.schema();
    let (windowed, _) = lowering.helper(vec![window_input], window)?;
    let narrowed = narrow(
        &windowed_schema,
        width,
        Some(windowed_schema.fields.len() - 1),
        output,
    )?;
    lowering.dag.add(lowering.id, vec![windowed], narrowed)?;
    Ok(())
}

/// What PromQL relabel `value` writes, from the expression the frontend
/// lowers `label_replace`, `label_join` or a constant label to.
fn relabel_value(
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

/// A Filter or Project whose expressions read other operators (PromQL
/// `scalar(v)`, SQL subqueries): each distinct read binds its value as a
/// column after the input's, then the expressions read those columns.
fn bind_scalar_reads(
    lowering: &mut Lowering<'_>,
    operator: &NonASAPOp<PhysicalASAPNodeId>,
    inputs: &[NodeId],
    schemas: &[SchemaRef],
    scalar_reads: usize,
    output: &SchemaRef,
) -> Result<(), Error> {
    let ([input_id], [input, ..]) = (&inputs[..inputs.len() - scalar_reads], schemas) else {
        return Err(invalid("a scalar read binds into one input"));
    };
    let width = input.fields.len();
    let schema_of = |producer: PhysicalASAPNodeId| {
        inputs
            .iter()
            .position(|&input| input == producer as NodeId)
            .map(|index| schemas[index].clone())
            .ok_or_else(|| invalid(format!("scalar read of node {producer} has no input edge")))
    };
    // The distinct reads, in order: each its producer and form; an `IN`
    // read is distinct by its probe too.
    let mut reads: Vec<(PhysicalASAPNodeId, SubqueryRead, String)> = Vec::new();
    let mut error = None;
    let mut bind = |expr: &ScalarExpr<PhysicalASAPNodeId>| -> ScalarExpr<PhysicalASAPNodeId> {
        let read = || -> Result<(PhysicalASAPNodeId, SubqueryRead, String, bool), Error> {
            Ok(match expr {
                ScalarExpr::PromqlScalarFromVector(producer) => {
                    let schema = schema_of(*producer)?;
                    let floats: Vec<_> = (0..schema.fields.len())
                        .filter(|&i| {
                            plain(&schema, i).is_ok_and(|t| t == (&DataType::Float64, false))
                        })
                        .collect();
                    let [value] = floats[..] else {
                        return Err(invalid("scalar() reads a vector of one sample value"));
                    };
                    (
                        *producer,
                        SubqueryRead::VectorScalar { value },
                        "scalar".into(),
                        false,
                    )
                }
                ScalarExpr::ScalarSubquery(producer) => {
                    (*producer, SubqueryRead::Scalar, "scalar".into(), false)
                }
                ScalarExpr::Exists { subquery, negated } => {
                    (*subquery, SubqueryRead::Exists, "exists".into(), *negated)
                }
                ScalarExpr::InSubquery {
                    expr,
                    subquery,
                    negated,
                } => {
                    let probe = expression(expr, input)?;
                    let key = format!("in {expr:?}");
                    (*subquery, SubqueryRead::In { probe }, key, *negated)
                }
                _ => unreachable!("an operator read"),
            })
        };
        let (producer, form, key, negated) = match read() {
            Ok(read) => read,
            Err(e) => {
                error.get_or_insert(e);
                return ScalarExpr::Literal(ScalarValue::Null);
            }
        };
        let index = match reads
            .iter()
            .position(|(p, _, k)| *p == producer && *k == key)
        {
            Some(index) => index,
            None => {
                reads.push((producer, form, key));
                reads.len() - 1
            }
        };
        let column = ScalarExpr::Column(width + index);
        match negated {
            true => ScalarExpr::Not(Box::new(column)),
            false => column,
        }
    };
    let bound = match operator {
        NonASAPOp::Filter { pred, .. } => vec![pred.0.replace_operator_reads(&mut bind)],
        NonASAPOp::Project { cols, .. } => cols
            .iter()
            .map(|col| col.expr.replace_operator_reads(&mut bind))
            .collect(),
        _ => unreachable!("a Filter or a Project"),
    };
    if let Some(error) = error {
        return Err(error);
    }
    let (mut current, mut current_schema) = (*input_id, input.clone());
    for (index, (producer, form, _)) in reads.into_iter().enumerate() {
        let operator = Operator::bind_subquery(
            current_schema,
            schema_of(producer)?,
            form,
            &format!("$read_{index}"),
        )?;
        (current, current_schema) = lowering.helper(vec![current, producer as NodeId], operator)?;
    }
    match operator {
        NonASAPOp::Filter { .. } => {
            let filter = Operator::filter(
                current_schema.clone(),
                expression(&bound[0], &current_schema)?,
            )?;
            let (filtered, _) = lowering.helper(vec![current], filter)?;
            let narrowed = narrow(&current_schema, width, None, output)?;
            lowering.dag.add(lowering.id, vec![filtered], narrowed)?;
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
            lowering.dag.add(
                lowering.id,
                vec![current],
                project.with_output_schema(output.clone())?,
            )?;
        }
    }
    Ok(())
}
