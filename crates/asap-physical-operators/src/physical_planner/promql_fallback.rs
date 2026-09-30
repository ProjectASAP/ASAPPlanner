//! Compile a retained PromQL subtree (`Fallback`) from its typed expression.
//! The deployment supplies the raw series of each selector; the Planner
//! computes selection, range functions, subqueries, matching and aggregation.
use super::*;
use crate::operators::SubquerySteps;
use planner_types::post_asap::execution_data_state::lift_plain;
use planner_types::pre_asap::{AtModifier, VectorMatchKind};

/// Input slot for the raw series read by the `selector`th selector (in
/// [`raw_series`] order) of Fallback node `node`. The node's own ID names its
/// computed output, so the raw rows need another.
pub fn raw_series_input(node: NodeId, selector: usize) -> NodeId {
    node | ((selector as u64 + 1) << 32)
}

/// The Fallback node that owns a raw-series input slot.
pub(super) fn raw_series_owner(slot: NodeId) -> Option<NodeId> {
    (slot >> 32 != 0).then_some(slot & u64::from(u32::MAX))
}

/// A selector expression and its raw-series row schema.
pub type Selector = (QueryExpr, Schema);

/// The selectors a Fallback expression reads, left to right, and the row
/// schema of the raw series the deployment supplies for each at
/// [`raw_series_input`]. The rows must cover the selector's window at every
/// evaluation instant `T`, or at its `@` time: `(T - offset - range, T - offset]`;
/// under a subquery `[R:S] offset O` that is `(T - O - R - offset - range, T - O - offset]`.
pub fn raw_series(expression: &QueryExpr) -> Result<Vec<Selector>, Error> {
    Ok(lower(expression)?.selectors)
}

/// An operator input: a selector's raw rows or an earlier step.
pub(super) enum Input {
    Raw(usize),
    Step(usize),
}

/// Operators computing an expression; the last step is its result.
#[derive(Default)]
pub(super) struct Lowering {
    pub selectors: Vec<Selector>,
    pub steps: Vec<(Operator, Vec<Input>)>,
}

pub(super) fn lower(expression: &QueryExpr) -> Result<Lowering, Error> {
    let mut lowering = Lowering::default();
    lowering.value(expression)?;
    Ok(lowering)
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

/// A fixed `@` time. `start()`/`end()` depend on the deployment's range query.
fn at(shift: &planner_types::pre_asap::TimeShift) -> Result<Option<i64>, Error> {
    match shift.at {
        None => Ok(None),
        Some(AtModifier::Timestamp(at)) => Ok(Some(at)),
        Some(_) => Err(invalid("@ start() and @ end() depend on the range query")),
    }
}

/// `TimeRange { range, [TimeShift { offset, @ }], Scan }`: range, offset, `@`.
fn selector(expression: &QueryExpr) -> Result<(i64, i64, Option<i64>), Error> {
    let QueryExpr::TimeRange { range, child } = expression else {
        return Err(invalid("PromQL operand must be a series selector"));
    };
    let (offset, at, scan) = match child.as_ref() {
        QueryExpr::TimeShift { shift, child } => (shift.offset_ms, at(shift)?, child.as_ref()),
        scan => (0, None, scan),
    };
    if !matches!(scan, QueryExpr::Scan { .. }) {
        return Err(invalid("PromQL selector must read one scan"));
    }
    Ok((millis(range)?, offset, at))
}

/// PromQL scalar-valued expressions have no labels to match. A binary
/// operator is scalar-valued when both operands are.
pub(super) fn scalar(expression: &QueryExpr) -> bool {
    match expression {
        QueryExpr::PromqlScalarBridge(_)
        | QueryExpr::PromqlScalarFromVector(_)
        | QueryExpr::EvalTimestamp => true,
        QueryExpr::BinaryOp { lhs, rhs, .. } => scalar(lhs) && scalar(rhs),
        _ => false,
    }
}

impl Lowering {
    fn schema(&self, input: &Input) -> Schema {
        match input {
            Input::Raw(i) => self.selectors[*i].1.clone(),
            Input::Step(i) => self.steps[*i].0.schema(),
        }
    }

    fn add(&mut self, operator: Operator, inputs: Vec<Input>) -> Input {
        self.steps.push((operator, inputs));
        Input::Step(self.steps.len() - 1)
    }

    /// Conform `operator` to the logical schema of the expression it computes.
    fn push(
        &mut self,
        operator: Operator,
        inputs: Vec<Input>,
        logical: &QueryExpr,
    ) -> Result<Input, Error> {
        Ok(self.add(operator.with_output_schema(declared(logical)?)?, inputs))
    }

    fn read(&mut self, selector: &QueryExpr) -> Result<Input, Error> {
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
        self.selectors.push((selector.clone(), schema));
        Ok(Input::Raw(self.selectors.len() - 1))
    }

    /// An instant vector, or a scalar for scalar-valued expressions.
    fn value(&mut self, expression: &QueryExpr) -> Result<Input, Error> {
        match expression {
            QueryExpr::Concat { children, .. } => {
                if !children.iter().all(|branch| matches!(branch,
                    QueryExpr::PromqlRelabel { child, .. } if matches!(child.as_ref(),
                        QueryExpr::Aggregate { measures, .. } if matches!(measures.as_slice(), [AggIntent::HistogramQuantile { .. }])))) {
                    return Err(invalid("PromQL concatenation requires classic histogram quantile branches"));
                }
                let inputs = children
                    .iter()
                    .map(|child| self.value(child))
                    .collect::<Result<Vec<_>, _>>()?;
                let output = declared(expression)?;
                if inputs.iter().any(|input| self.schema(input) != output) {
                    return Err(invalid(
                        "concatenated PromQL branches require equal schemas",
                    ));
                }
                let union = self.add(Operator::union(output.clone(), inputs.len())?, inputs);
                // Multi-quantile branches drop the metric name and form one vector.
                self.push(
                    Operator::series_without_name(output)?,
                    vec![union],
                    expression,
                )
            }
            QueryExpr::PromqlRelabel { dst, value, child } => {
                let step = self.value(child)?;
                let input = self.schema(&step);
                let (replacement, source_regex) = match value.as_ref() {
                    QueryExpr::Literal(planner_types::pre_asap::ScalarValue::Utf8(value)) => {
                        (value.clone(), None)
                    }
                    QueryExpr::FunctionCall { name, args } if name == "label_replace" => {
                        let [QueryExpr::Column(source), QueryExpr::Literal(planner_types::pre_asap::ScalarValue::Utf8(pattern)), QueryExpr::Literal(planner_types::pre_asap::ScalarValue::Utf8(
                            replacement,
                        ))] = args.as_slice()
                        else {
                            return Err(invalid("invalid label_replace arguments"));
                        };
                        let source = input
                            .fields
                            .get(*source)
                            .ok_or_else(|| invalid("label_replace source missing"))?
                            .name
                            .clone();
                        (replacement.clone(), Some((source, pattern.clone())))
                    }
                    _ => return Err(invalid("unsupported PromQL label rewrite")),
                };
                let operator = Operator::series_relabel(
                    input,
                    declared(expression)?,
                    dst.clone(),
                    replacement,
                    source_regex,
                )?;
                self.push(operator, vec![step], expression)
            }
            QueryExpr::TimeRange { .. } => {
                let (range, offset, at) = selector(expression)?;
                let input = self.read(expression)?;
                let schema = self.schema(&input);
                self.push(
                    Operator::series_window(schema, None, range, offset, at, None)?,
                    vec![input],
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
                let step = self.range_function(function, child, expression)?;
                if matches!(function, AggIntent::LastOverTime) {
                    return Ok(step);
                }
                // Other range functions drop the name; equal label sets then error.
                let input = self.schema(&step);
                Ok(self.add(Operator::series_without_name(input)?, vec![step]))
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
                let input = self.value(child)?;
                self.aggregate(input, measure, keys, expression)
            }
            QueryExpr::Sort {
                keys,
                partition_by,
                child,
            } => {
                let step = self.value(child)?;
                let input = self.schema(&step);
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
                self.push(Operator::sort(input, keys, groups)?, vec![step], expression)
            }
            QueryExpr::Limit { n, offset, child } => {
                let step = self.value(child)?;
                let input = self.schema(&step);
                // `topk by (...)` partitions through the Sort it limits.
                let groups = match child.as_ref() {
                    QueryExpr::Sort { partition_by, .. } => groups(&input, partition_by)?,
                    _ => vec![],
                };
                self.push(
                    Operator::limit(input, *n as u64, *offset as u64, groups)?,
                    vec![step],
                    expression,
                )
            }
            QueryExpr::BinaryOp {
                op,
                lhs,
                rhs,
                vector_match,
            } => {
                let sides = vec![self.value(lhs)?, self.value(rhs)?];
                let operator = planner_types::post_asap::BinaryOperator {
                    kind: op.clone(),
                    vector_match: vector_match.clone(),
                    checked_relative_division: false,
                    checked_finite_division: false,
                };
                let binary = Operator::series_binary(
                    self.schema(&sides[0]),
                    self.schema(&sides[1]),
                    operator,
                    [scalar(lhs), scalar(rhs)],
                )?;
                self.push(binary, sides, expression)
            }
            QueryExpr::PromqlScalarFromVector(child) => {
                let step = self.value(child)?;
                let input = self.schema(&step);
                let value = named_column(&input, &ColumnRef::SampleValue)?;
                self.push(
                    Operator::vector_to_scalar(input, value)?,
                    vec![step],
                    expression,
                )
            }
            QueryExpr::PromqlVectorFromScalar(child) => {
                let step = self.value(child)?;
                let input = self.schema(&step);
                Ok(self.add(
                    Operator::scope_timestamp(input, declared(expression)?)?,
                    vec![step],
                ))
            }
            QueryExpr::EvalTimestamp => self.push(Operator::evaluation_time(), vec![], expression),
            QueryExpr::PromqlScalarBridge(_) => {
                let value = row_values::scalar_literal(expression)
                    .ok_or_else(|| invalid("PromQL scalar must be a literal"))?;
                self.push(
                    Operator::scalar(crate::values::Value::Float64(value), DataType::Float64)?,
                    vec![],
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
    ) -> Result<Input, Error> {
        let function = unbound(function)?;
        let (subquery, offset, at_ms) = match matrix {
            QueryExpr::TimeShift { shift, child } => (child.as_ref(), shift.offset_ms, at(shift)?),
            other => (other, 0, None),
        };
        let QueryExpr::PromqlSubquery {
            range: outer,
            resolution,
            child,
        } = subquery
        else {
            let (range, offset, at) = selector(matrix)?;
            let input = self.read(matrix)?;
            let schema = self.schema(&input);
            return self.push(
                Operator::series_window(schema, Some(function), range, offset, at, None)?,
                vec![input],
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
            at_ms,
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
        let (range, inner_offset, inner_at) = selector(selected)?;
        let raw = self.read(selected)?;
        let schema = self.schema(&raw);
        let mut step = self.push(
            Operator::series_window(
                schema,
                inner.clone(),
                range,
                inner_offset,
                inner_at,
                Some(steps),
            )?,
            vec![raw],
            child,
        )?;
        // The inner function drops the name too. A series repeats across
        // steps, so this rewrite does not check for equal label sets.
        if inner.is_some() && !matches!(inner, Some(AggIntent::LastOverTime)) {
            let input = self.schema(&step);
            let relabel = Operator::series_labels(input, VectorMatchKind::Ignoring, vec![])?;
            step = self.add(relabel, vec![step]);
        }
        let input = self.schema(&step);
        self.push(
            Operator::series_window(input, Some(function), steps.range_ms, offset, at_ms, None)?,
            vec![step],
            logical,
        )
    }

    /// Cross-series aggregation. A global aggregate groups by one constant so
    /// that no input series yields an empty vector, not one row.
    fn aggregate(
        &mut self,
        mut step: Input,
        measure: &AggIntent,
        keys: &GroupKeys,
        logical: &QueryExpr,
    ) -> Result<Input, Error> {
        let mut input = self.schema(&step);
        if let AggIntent::HistogramQuantile { q, le } = measure {
            if !keys.is_without() || keys.keys() != [*le] {
                return Err(invalid("histogram_quantile must group without (le)"));
            }
            let operator = Operator::series_histogram_quantile(input, *q, *le)?;
            return self.push(operator, vec![step], logical);
        }
        let value = input
            .fields
            .iter()
            .enumerate()
            .filter(|(_, f)| f.dtype == SummaryFamilyType::Plain(DataType::Float64))
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        let [value] = value.as_slice() else {
            return Err(invalid("PromQL aggregation requires one Float64 value"));
        };
        let value = *value;
        let reduction = match measure {
            AggIntent::Sum { col: None } => Reduction::Sum(value),
            AggIntent::Avg { col: None } => Reduction::Avg(value),
            AggIntent::Min { col: None } => Reduction::Min(value),
            AggIntent::Max { col: None } => Reduction::Max(value),
            AggIntent::Count { .. } => Reduction::Count,
            _ => return Err(invalid("vector aggregate has no native lowering")),
        };
        let mut groups = if keys.is_without() {
            // Group by every remaining label, including the rewritten identity.
            let excluded = keys.keys();
            if excluded.iter().any(|&i| i >= input.fields.len()) {
                return Err(invalid("grouping column out of range"));
            }
            let names = excluded.iter().map(|&i| input.fields[i].name.clone());
            let relabel =
                Operator::series_labels(input.clone(), VectorMatchKind::Ignoring, names.collect())?;
            step = self.add(relabel, vec![step]);
            (0..input.fields.len())
                .filter(|&i| Some(i) != input.time_index && i != value && !excluded.contains(&i))
                .collect()
        } else {
            groups(&input, keys)?
        };
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
            step = self.add(project, vec![step]);
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
        let step = self.add(aggregate, vec![step]);
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
        self.push(Operator::project(actual, columns)?, vec![step], logical)
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
        AggIntent::IRate => AggIntent::IRate,
        AggIntent::IDelta => AggIntent::IDelta,
        AggIntent::Changes => AggIntent::Changes,
        AggIntent::Resets => AggIntent::Resets,
        AggIntent::LastOverTime => AggIntent::LastOverTime,
        AggIntent::Quantile {
            col: None,
            q,
            accuracy,
        } => AggIntent::Quantile {
            col: None,
            q: *q,
            accuracy: accuracy.clone(),
        },
        _ => return Err(invalid("unsupported PromQL range function")),
    })
}
