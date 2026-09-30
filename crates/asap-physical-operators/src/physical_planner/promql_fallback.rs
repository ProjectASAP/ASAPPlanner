//! Compile a retained PromQL subtree (`Fallback`) from its typed expression.
//! The deployment supplies the raw series of its one selector; the Planner
//! computes selection, range functions, subqueries and aggregation.
use super::*;
use crate::operators::SubquerySteps;
use planner_types::post_asap::execution_data_state::lift_plain;

/// Input slot for the raw series read by Fallback node `node`'s selector.
/// The node's own ID names its computed output, so the raw rows need another.
pub fn raw_series_input(node: NodeId) -> NodeId {
    node | (1 << 32)
}

/// The Fallback node that owns a raw-series input slot.
pub(super) fn raw_series_owner(slot: NodeId) -> Option<NodeId> {
    (slot >> 32 == 1).then_some(slot & u64::from(u32::MAX))
}

/// A selector expression and its raw-series row schema.
pub type Selector = (QueryExpr, Schema);

/// The selector a Fallback expression reads, and the row schema of the raw
/// series the deployment supplies at [`raw_series_input`]. `None` means the
/// expression reads no series. The rows must cover the selector's window at
/// every evaluation instant; under a subquery `[R:S] offset O` that is
/// `(T - O - R - offset - range, T - O - offset]`.
pub fn raw_series(expression: &QueryExpr) -> Result<Option<Selector>, Error> {
    Ok(lower(expression)?.0)
}

/// Operators computing `expression`, in order, after its raw-series input.
pub(super) fn lower(expression: &QueryExpr) -> Result<(Option<Selector>, Vec<Operator>), Error> {
    let mut chain = Chain::default();
    chain.value(expression)?;
    Ok((chain.leaf, chain.operators))
}

fn declared(expression: &QueryExpr) -> Result<Schema, Error> {
    let schema = expression
        .output_schema()
        .map_err(|error| invalid(error.to_string()))?;
    Ok(Arc::new(lift_plain(&schema)))
}

fn millis(duration: &std::time::Duration) -> Result<i64, Error> {
    i64::try_from(duration.as_millis()).map_err(|_| invalid("PromQL duration exceeds Int64"))
}

/// `TimeRange { range, [TimeShift { offset }], Scan }`: range and offset.
fn selector(expression: &QueryExpr) -> Result<(i64, i64), Error> {
    let QueryExpr::TimeRange { range, child } = expression else {
        return Err(invalid("PromQL operand must be a series selector"));
    };
    let (offset, scan) = match child.as_ref() {
        QueryExpr::TimeShift { shift, child } if shift.at.is_none() => {
            (shift.offset_ms, child.as_ref())
        }
        scan => (0, scan),
    };
    if !matches!(scan, QueryExpr::Scan { .. }) {
        return Err(invalid(
            "PromQL selector must read one scan; @ is unsupported",
        ));
    }
    Ok((millis(range)?, offset))
}

#[derive(Default)]
struct Chain {
    leaf: Option<Selector>,
    operators: Vec<Operator>,
}

impl Chain {
    fn schema(&self) -> Result<Schema, Error> {
        self.operators
            .last()
            .map(Operator::schema)
            .or_else(|| self.leaf.as_ref().map(|(_, schema)| schema.clone()))
            .ok_or_else(|| invalid("PromQL operator has no input"))
    }

    /// Conform `operator` to the logical schema of the expression it computes.
    fn push(&mut self, operator: Operator, logical: &QueryExpr) -> Result<(), Error> {
        self.operators
            .push(operator.with_output_schema(declared(logical)?)?);
        Ok(())
    }

    fn read(&mut self, selector: &QueryExpr) -> Result<Schema, Error> {
        let schema = declared(selector)?;
        if !schema
            .fields
            .iter()
            .any(|f| f.name == promql_rows::SERIES_IDENTITY_COLUMN)
        {
            return Err(invalid(
                "PromQL fallback requires the complete series identity",
            ));
        }
        if self
            .leaf
            .replace((selector.clone(), schema.clone()))
            .is_some()
        {
            return Err(invalid("PromQL fallback reads more than one selector"));
        }
        Ok(schema)
    }

    /// An instant vector, or a scalar for scalar-valued expressions.
    fn value(&mut self, expression: &QueryExpr) -> Result<(), Error> {
        match expression {
            QueryExpr::TimeRange { .. } => {
                let (range, offset) = selector(expression)?;
                let input = self.read(expression)?;
                self.push(
                    Operator::series_window(input, None, range, offset, None)?,
                    expression,
                )
            }
            QueryExpr::Aggregate {
                reduction: planner_types::pre_asap::Reduction::PerEntity,
                measures,
                having: None,
                child,
                ..
            } => {
                let [function] = measures.as_slice() else {
                    return Err(invalid("range function requires one measure"));
                };
                self.range_function(function, child, expression)
            }
            QueryExpr::Aggregate {
                reduction: planner_types::pre_asap::Reduction::Reduce(keys),
                measures,
                having: None,
                child,
                ..
            } => {
                let [measure] = measures.as_slice() else {
                    return Err(invalid("vector aggregation requires one measure"));
                };
                self.value(child)?;
                self.aggregate(measure, keys, expression)
            }
            QueryExpr::Sort {
                keys,
                partition_by,
                child,
            } => {
                self.value(child)?;
                let input = self.schema()?;
                let keys = keys
                    .iter()
                    .map(|key| match key.expr {
                        QueryExpr::Column(column) => Ok(SortKey {
                            column,
                            descending: !key.ascending,
                            nulls_first: key.nulls_first,
                        }),
                        _ => Err(invalid("sort key must be a column")),
                    })
                    .collect::<Result<_, _>>()?;
                let groups = groups(&input, partition_by)?;
                self.push(Operator::sort(input, keys, groups)?, expression)
            }
            QueryExpr::Limit { n, offset, child } => {
                self.value(child)?;
                let input = self.schema()?;
                // `topk by (...)` partitions through the Sort it limits.
                let groups = match child.as_ref() {
                    QueryExpr::Sort { partition_by, .. } => groups(&input, partition_by)?,
                    _ => vec![],
                };
                self.push(
                    Operator::limit(input, *n as u64, *offset as u64, groups)?,
                    expression,
                )
            }
            QueryExpr::BinaryOp {
                op: planner_types::pre_asap::BinaryOpKind::Arithmetic(op),
                lhs,
                rhs,
                vector_match: None,
            } => {
                let (vector, literal, literal_left) = match (
                    row_values::scalar_literal(lhs),
                    row_values::scalar_literal(rhs),
                ) {
                    (None, Some(value)) => (lhs, value, false),
                    (Some(value), None) => (rhs, value, true),
                    _ => {
                        return Err(invalid(
                            "PromQL fallback arithmetic requires one literal operand",
                        ))
                    }
                };
                self.value(vector)?;
                let input = self.schema()?;
                let value = named_column(&input, &ColumnRef::SampleValue)?;
                let literal = Expression::Literal {
                    value: crate::values::Value::Float64(literal),
                    dtype: DataType::Float64,
                };
                let operator = planner_types::post_asap::BinaryOperator {
                    kind: planner_types::pre_asap::BinaryOpKind::Arithmetic(op.clone()),
                    vector_match: None,
                    checked_relative_division: false,
                    checked_finite_division: false,
                };
                let (left, right) = if literal_left {
                    (literal, Expression::Column(value))
                } else {
                    (Expression::Column(value), literal)
                };
                let columns = input
                    .fields
                    .iter()
                    .enumerate()
                    .map(|(i, field)| {
                        let expression = if i == value {
                            Expression::Binary {
                                operator: operator.clone(),
                                left: Box::new(left.clone()),
                                right: Box::new(right.clone()),
                            }
                        } else {
                            Expression::Column(i)
                        };
                        (field.name.clone(), expression)
                    })
                    .collect();
                self.push(Operator::project(input, columns)?, expression)
            }
            QueryExpr::PromqlScalarFromVector(child) => {
                self.value(child)?;
                let input = self.schema()?;
                let value = named_column(&input, &ColumnRef::SampleValue)?;
                self.push(Operator::vector_to_scalar(input, value)?, expression)
            }
            QueryExpr::PromqlVectorFromScalar(child) => {
                self.value(child)?;
                let input = self.schema()?;
                self.operators
                    .push(Operator::scope_timestamp(input, declared(expression)?)?);
                Ok(())
            }
            QueryExpr::PromqlScalarBridge(_) => {
                let value = row_values::scalar_literal(expression)
                    .ok_or_else(|| invalid("PromQL scalar must be a literal"))?;
                self.push(
                    Operator::scalar(crate::values::Value::Float64(value), DataType::Float64)?,
                    expression,
                )
            }
            _ => Err(invalid("PromQL expression has no native fallback lowering")),
        }
    }

    /// `function(matrix)`, where the matrix is a range selector or a subquery.
    fn range_function(
        &mut self,
        function: &AggIntent,
        matrix: &QueryExpr,
        logical: &QueryExpr,
    ) -> Result<(), Error> {
        let function = unbound(function)?;
        let (subquery, offset) = match matrix {
            QueryExpr::TimeShift { shift, child } if shift.at.is_none() => {
                (child.as_ref(), shift.offset_ms)
            }
            other => (other, 0),
        };
        let QueryExpr::PromqlSubquery {
            range: outer,
            resolution,
            child,
        } = subquery
        else {
            let (range, offset) = selector(matrix)?;
            let input = self.read(matrix)?;
            return self.push(
                Operator::series_window(input, Some(function), range, offset, None)?,
                logical,
            );
        };
        let step = resolution.as_ref().ok_or_else(|| {
            invalid("subquery resolution defaults to the deployment evaluation interval")
        })?;
        let steps = SubquerySteps {
            range_ms: millis(outer)?,
            step_ms: millis(step)?,
            offset_ms: offset,
        };
        // Each step evaluates a per-series selection or range function.
        let (inner, selected) = match child.as_ref() {
            QueryExpr::Aggregate {
                reduction: planner_types::pre_asap::Reduction::PerEntity,
                measures,
                having: None,
                child: selected,
                ..
            } => match measures.as_slice() {
                [inner] => (Some(unbound(inner)?), selected.as_ref()),
                _ => return Err(invalid("range function requires one measure")),
            },
            selected => (None, selected),
        };
        let (range, inner_offset) = selector(selected)?;
        let input = self.read(selected)?;
        self.push(
            Operator::series_window(input, inner, range, inner_offset, Some(steps))?,
            child,
        )?;
        let input = self.schema()?;
        self.push(
            Operator::series_window(input, Some(function), steps.range_ms, offset, None)?,
            logical,
        )
    }

    /// Cross-series aggregation. A global aggregate groups by one constant so
    /// that no input series yields an empty vector, not one row.
    fn aggregate(
        &mut self,
        measure: &AggIntent,
        keys: &GroupKeys,
        logical: &QueryExpr,
    ) -> Result<(), Error> {
        let mut input = self.schema()?;
        let value = named_column(&input, &ColumnRef::SampleValue)?;
        let reduction = match measure {
            AggIntent::Sum { col: None } => Reduction::Sum(value),
            AggIntent::Avg { col: None } => Reduction::Avg(value),
            AggIntent::Min { col: None } => Reduction::Min(value),
            AggIntent::Max { col: None } => Reduction::Max(value),
            AggIntent::Count { .. } => Reduction::Count,
            _ => return Err(invalid("vector aggregate has no native lowering")),
        };
        let mut groups = groups(&input, keys)?;
        let global = groups.is_empty();
        if global {
            let mut columns = (0..input.fields.len())
                .map(|i| (input.fields[i].name.clone(), Expression::Column(i)))
                .collect::<Vec<_>>();
            columns.push((
                "$promql_global_group".into(),
                Expression::Literal {
                    value: crate::values::Value::Utf8("".into()),
                    dtype: DataType::Utf8,
                },
            ));
            let project = Operator::project(input, columns)?;
            input = project.schema();
            groups = vec![input.fields.len() - 1];
            self.operators.push(project);
        }
        let output = declared(logical)?;
        let name = output
            .fields
            .last()
            .ok_or_else(|| invalid("aggregate output lacks a value"))?
            .name
            .clone();
        let aggregate = Operator::aggregate(input, groups, vec![(name, reduction)])?;
        let actual = aggregate.schema();
        self.operators.push(aggregate);
        // Drop the constant group; convert counts where PromQL declares Float64.
        let skip = usize::from(global);
        let columns = actual.fields[skip..]
            .iter()
            .zip(&output.fields)
            .enumerate()
            .map(|(i, (field, declared))| {
                let column = i + skip;
                let expression = if field.dtype != declared.dtype {
                    Expression::ExactFloat64(column)
                } else {
                    Expression::Column(column)
                };
                (field.name.clone(), expression)
            })
            .collect();
        self.push(Operator::project(actual, columns)?, logical)
    }
}

fn unbound(intent: &AggIntent) -> Result<AggIntent<ColumnRef>, Error> {
    Ok(match intent {
        AggIntent::Rate => AggIntent::Rate,
        AggIntent::Increase => AggIntent::Increase,
        AggIntent::Delta => AggIntent::Delta,
        AggIntent::Count { accuracy } => AggIntent::Count {
            accuracy: accuracy.clone(),
        },
        AggIntent::Sum { col: None } => AggIntent::Sum { col: None },
        AggIntent::Avg { col: None } => AggIntent::Avg { col: None },
        AggIntent::Min { col: None } => AggIntent::Min { col: None },
        AggIntent::Max { col: None } => AggIntent::Max { col: None },
        _ => return Err(invalid("unsupported PromQL range function")),
    })
}
