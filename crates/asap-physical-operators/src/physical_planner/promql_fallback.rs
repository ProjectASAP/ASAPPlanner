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
pub type Selector = (OperatorNode, Schema);

/// The selectors a Fallback expression reads, left to right, and the row
/// schema of the raw series the deployment supplies for each at
/// [`raw_series_input`]. The rows must cover the selector's window at every
/// evaluation instant `T`, or at its `@` time: `(T - offset - range, T - offset]`;
/// under a subquery `[R:S] offset O` that is `(T - O - R - offset - range, T - O - offset]`.
pub fn raw_series(expression: &OperatorNode) -> Result<Vec<Selector>, Error> {
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

pub(super) fn lower(expression: &OperatorNode) -> Result<Lowering, Error> {
    let mut lowering = Lowering::default();
    lowering.value(expression)?;
    Ok(lowering)
}

/// Compile a standalone scalar expression and expose its real series dependencies.
/// Input slots use root 0; no logical wrapper node is introduced.
pub fn compile_scalar_root(
    expr: &ScalarExpr,
) -> Result<(CompiledPhysicalDag, Vec<Selector>), Error> {
    let mut lowering = Lowering::default();
    lowering.scalar_value(expr)?;
    let mut inputs = BTreeMap::new();
    for (i, (_, schema)) in lowering.selectors.iter().enumerate() {
        inputs.insert(
            raw_series_input(0, i),
            InputContract::bounded(schema.clone()),
        );
    }
    let last = lowering.steps.len() - 1;
    let mut operators = BTreeMap::new();
    for (i, (operator, dependencies)) in lowering.steps.into_iter().enumerate() {
        let id = if i == last { 0 } else { i as u64 + 1 };
        let dependencies = dependencies
            .into_iter()
            .map(|input| match input {
                Input::Raw(i) => raw_series_input(0, i),
                Input::Step(i) => i as u64 + 1,
            })
            .collect();
        operators.insert(id, (dependencies, operator));
    }
    Ok((
        CompiledPhysicalDag::from_operators(inputs, operators, vec![0])?,
        lowering.selectors,
    ))
}

fn declared(expression: &OperatorNode) -> Result<Schema, Error> {
    let schema = expression.schema.clone();
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
        Some(AtModifier::Start | AtModifier::End) => Ok(None),
    }
}

fn range_anchor(expression: &OperatorNode) -> Option<AtModifier> {
    match expression.expect_non_asap() {
        NonASAPOp::TimeRange { child, .. } => range_anchor(child),
        NonASAPOp::TimeShift { shift, .. } => shift
            .at
            .filter(|at| matches!(at, AtModifier::Start | AtModifier::End)),
        _ => None,
    }
}

/// `TimeRange { range, [TimeShift { offset, @ }], Scan }`: range, offset, `@`.
fn selector(expression: &OperatorNode) -> Result<(i64, i64, Option<i64>), Error> {
    let NonASAPOp::TimeRange { range, child, .. } = expression.expect_non_asap() else {
        return Err(invalid("PromQL operand must be a series selector"));
    };
    let (offset, at, scan) = match child.expect_non_asap() {
        NonASAPOp::TimeShift { shift, child } => {
            (shift.offset_ms, at(shift)?, child.expect_non_asap())
        }
        scan => (0, None, scan),
    };
    if !matches!(scan, NonASAPOp::Scan { .. }) {
        return Err(invalid("PromQL selector must read one scan"));
    }
    Ok((millis(range)?, offset, at))
}

/// PromQL scalar-valued expressions have no labels to match. A binary
/// operator is scalar-valued when both operands are.
pub(super) fn scalar(expression: &OperatorNode) -> bool {
    expression.result_kind == planner_types::ir::OperatorResultKind::Scalar
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
        logical: &OperatorNode,
    ) -> Result<Input, Error> {
        Ok(self.add(operator.with_output_schema(declared(logical)?)?, inputs))
    }

    fn read(&mut self, selector: &OperatorNode) -> Result<Input, Error> {
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
    fn value(&mut self, expression: &OperatorNode) -> Result<Input, Error> {
        match expression.expect_non_asap() {
            NonASAPOp::Concat { children, .. } => {
                if !children.iter().all(|branch| matches!(branch.expect_non_asap(),
                    NonASAPOp::PromqlRelabel { child, .. } if matches!(child.expect_non_asap(),
                        NonASAPOp::Aggregate { measures, .. } if matches!(measures.as_slice(), [AggIntent::HistogramQuantile { .. }])))) {
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
            NonASAPOp::PromqlRelabel { dst, value, child } => {
                let step = self.value(child)?;
                let input = self.schema(&step);
                let (replacement, source_regex) = match value {
                    ScalarExpr::Literal(planner_types::pre_asap::ScalarValue::Utf8(value)) => {
                        (value.clone(), None)
                    }
                    ScalarExpr::FunctionCall { name, args } if name == "label_replace" => {
                        let [ScalarExpr::Column(source), ScalarExpr::Literal(planner_types::pre_asap::ScalarValue::Utf8(
                            pattern,
                        )), ScalarExpr::Literal(planner_types::pre_asap::ScalarValue::Utf8(
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
            NonASAPOp::TimeRange { .. } => {
                let (range, offset, at) = selector(expression)?;
                let input = self.read(expression)?;
                let schema = self.schema(&input);
                self.push(
                    Operator::series_window(schema, None, range, offset, at, None)?
                        .with_series_range_bounds(range_anchor(expression), None)?,
                    vec![input],
                    expression,
                )
            }
            NonASAPOp::Aggregate {
                reduction: planner_types::pre_asap::Reduction::PerEntity,
                measures,
                having: None,
                child,
                filters,
                ..
            } if filters.iter().all(Option::is_none) => {
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
            NonASAPOp::Aggregate {
                reduction: planner_types::pre_asap::Reduction::Reduce(keys),
                measures,
                having: None,
                child,
                filters,
                ..
            } if filters.iter().all(Option::is_none) => {
                let [measure] = measures.as_slice() else {
                    return Err(invalid("vector aggregation requires one measure"));
                };
                let input = self.value(child)?;
                self.aggregate(input, measure, keys, expression)
            }
            NonASAPOp::Project {
                cols,
                child,
                qualifier,
            } => {
                let value = planner_types::pre_asap::column_resolution::resolve_column_ref(
                    &ColumnRef::SampleValue,
                    &child.schema,
                )
                .map_err(|e| invalid(e.to_string()))?;
                let sample = cols
                    .iter()
                    .find(|col| {
                        col.alias.as_deref() == Some(child.schema.fields[value].name.as_str())
                    })
                    .ok_or_else(|| invalid("missing sample projection"))?;
                let keep_name = matches!(sample.expr, ScalarExpr::Negative { .. });
                let fields: Vec<_> = child
                    .schema
                    .fields
                    .iter()
                    .enumerate()
                    .filter(|(_, field)| keep_name || field.name != "__name__")
                    .collect();
                if qualifier.is_some() || cols.len() != fields.len() {
                    return Err(invalid("unsupported temporal projection shape"));
                }
                let mut computed = None;
                for (col, (index, field)) in cols.iter().zip(fields) {
                    if col.alias.as_deref() != Some(field.name.as_str()) {
                        return Err(invalid("unsupported temporal projection alias"));
                    }
                    if index == value {
                        computed = Some(col);
                    } else {
                        let expected = if !keep_name
                            && field.name == planner_types::pre_asap::schema::PROMQL_SERIES_IDENTITY
                        {
                            ScalarExpr::FunctionCall {
                                name: "promql_drop_metric_name".into(),
                                args: vec![ScalarExpr::Column(index)],
                            }
                        } else {
                            ScalarExpr::Column(index)
                        };
                        if col.expr != expected {
                            return Err(invalid("unsupported temporal projection expression"));
                        }
                    }
                }
                let computed = computed.ok_or_else(|| invalid("no computed sample"))?;
                if matches!(
                    computed.expr,
                    ScalarExpr::Negative { .. } | ScalarExpr::FunctionCall { .. }
                ) {
                    return self.pointwise_projection(cols, child, value, expression, keep_name);
                }
                self.sample_scalar_operation(&computed.expr, child, value, expression)
            }
            NonASAPOp::Filter { pred, child } => {
                let value = planner_types::pre_asap::column_resolution::resolve_column_ref(
                    &ColumnRef::SampleValue,
                    &child.schema,
                )
                .map_err(|e| invalid(e.to_string()))?;
                self.sample_scalar_operation(&pred.0, child, value, expression)
            }
            NonASAPOp::Sort {
                keys,
                partition_by,
                child,
            } => {
                let step = self.value(child)?;
                let input = self.schema(&step);
                let keys = keys
                    .iter()
                    .map(|key| match key.expr {
                        ScalarExpr::Column(column) => Ok(SortKey {
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
            NonASAPOp::Limit {
                n, offset, child, ..
            } => {
                let step = self.value(child)?;
                let input = self.schema(&step);
                // `topk by (...)` partitions through the Sort it limits.
                let groups = match child.expect_non_asap() {
                    NonASAPOp::Sort { partition_by, .. } => groups(&input, partition_by)?,
                    _ => vec![],
                };
                self.push(
                    Operator::limit(
                        input,
                        n.unwrap_or(usize::MAX) as u64,
                        *offset as u64,
                        groups,
                    )?,
                    vec![step],
                    expression,
                )
            }
            NonASAPOp::BinaryOp {
                operator,
                lhs,
                rhs,
                return_bool,
            } => {
                let sides = vec![self.value(lhs)?, self.value(rhs)?];
                let operator = crate::expressions::binary::BinaryOperator::from_logical(
                    operator,
                    *return_bool,
                );
                let binary = Operator::series_binary(
                    self.schema(&sides[0]),
                    self.schema(&sides[1]),
                    operator,
                    [scalar(lhs), scalar(rhs)],
                )?;
                self.push(binary, sides, expression)
            }
            NonASAPOp::PromqlVectorFromScalar(expr) => {
                let step = self.scalar_value(expr)?;
                let input = self.schema(&step);
                Ok(self.add(
                    Operator::scope_timestamp(input, declared(expression)?)?,
                    vec![step],
                ))
            }
            _ => Err(invalid("PromQL expression has no native fallback lowering")),
        }
    }

    fn pointwise_projection(
        &mut self,
        cols: &[planner_types::ir::ProjectItem],
        child: &OperatorNode,
        value: usize,
        output: &OperatorNode,
        keep_name: bool,
    ) -> Result<Input, Error> {
        let mut input = self.value(child)?;
        let mut projected = cols.to_vec();
        for col in &mut projected {
            if col.alias.as_deref() != Some(child.schema.fields[value].name.as_str()) {
                continue;
            }
            if let ScalarExpr::FunctionCall { name, args } = &mut col.expr {
                if planner_types::pre_asap::scalar_signature::promql_function_arity(name).is_none()
                    || args.first() != Some(&ScalarExpr::Column(value))
                {
                    return Err(invalid("unsupported pointwise function"));
                }
                for arg in args.iter_mut().skip(1) {
                    let scalar = self.scalar_value(arg)?;
                    let left = self.schema(&input);
                    let right = self.schema(&scalar);
                    let index = left.fields.len();
                    let mut schema = (*left).clone();
                    schema.fields.extend(right.fields.clone());
                    let join = Operator::relational_join(
                        left,
                        right,
                        planner_types::pre_asap::JoinKind::Inner,
                        &planner_types::ir::Predicate(ScalarExpr::Literal(
                            planner_types::pre_asap::ScalarValue::Boolean(true),
                        )),
                        Arc::new(schema),
                    )?;
                    input = self.add(join, vec![input, scalar]);
                    *arg = ScalarExpr::Column(index);
                }
                if name == "promql_clamp" {
                    let predicate = ScalarExpr::Not(Box::new(ScalarExpr::Compare {
                        left: Box::new(args[1].clone()),
                        right: Box::new(args[2].clone()),
                        op: planner_types::pre_asap::CompareOpKind::Gt,
                        semantics: planner_types::ir::ExprSemantics::Promql,
                    }));
                    let schema = self.schema(&input);
                    let predicate =
                        crate::expressions::CompiledExpression::compile(&predicate, &schema)?;
                    input = self.add(
                        Operator::filter(
                            schema,
                            crate::expressions::Expression::planner(predicate),
                        )?,
                        vec![input],
                    );
                }
            }
        }
        let schema = self.schema(&input);
        let columns = projected
            .iter()
            .map(|col| {
                Ok((
                    col.alias.clone().unwrap(),
                    crate::expressions::Expression::planner(
                        crate::expressions::CompiledExpression::compile(&col.expr, &schema)?,
                    ),
                ))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let project = Operator::project(schema, columns)?;
        let result = self.push(project, vec![input], output)?;
        if keep_name {
            Ok(result)
        } else {
            self.push(
                Operator::series_without_name(self.schema(&result))?,
                vec![result],
                output,
            )
        }
    }

    fn sample_scalar_operation(
        &mut self,
        expr: &ScalarExpr,
        child: &OperatorNode,
        value: usize,
        output: &OperatorNode,
    ) -> Result<Input, Error> {
        let (left, right, kind) = scalar_binary(expr)?;
        let (scalar, scalar_left) = match (left, right) {
            (ScalarExpr::Column(i), scalar) if *i == value => (scalar, false),
            (scalar, ScalarExpr::Column(i)) if *i == value => (scalar, true),
            _ => {
                return Err(invalid(
                    "sample projection requires one vector sample and one scalar",
                ))
            }
        };
        let vector = self.value(child)?;
        let scalar = self.scalar_value(scalar)?;
        let sides = if scalar_left {
            vec![scalar, vector]
        } else {
            vec![vector, scalar]
        };
        let operator = Operator::series_binary(
            self.schema(&sides[0]),
            self.schema(&sides[1]),
            kernel(kind),
            [scalar_left, !scalar_left],
        )?;
        self.push(operator, sides, output)
    }

    fn scalar_value(&mut self, expr: &ScalarExpr) -> Result<Input, Error> {
        match expr {
            ScalarExpr::Literal(planner_types::pre_asap::ScalarValue::Float64(value)) => Ok(self
                .add(
                    Operator::scalar(crate::values::Value::Float64(*value), DataType::Float64)?,
                    vec![],
                )),
            ScalarExpr::EvalTimestamp => Ok(self.add(Operator::evaluation_time(), vec![])),
            ScalarExpr::PromqlScalarFromVector(child) => {
                let step = self.value(child)?;
                let input = self.schema(&step);
                let values: Vec<_> = input
                    .fields
                    .iter()
                    .enumerate()
                    .filter(|(_, f)| f.dtype == FieldDataType::Plain(DataType::Float64))
                    .map(|(i, _)| i)
                    .collect();
                let [value] = values.as_slice() else {
                    return Err(invalid("scalar() requires one float sample column"));
                };
                let value = *value;
                Ok(self.add(Operator::vector_to_scalar(input, value)?, vec![step]))
            }
            ScalarExpr::Negative { expr, .. } => {
                let value = self.scalar_value(expr)?;
                let minus = self.scalar_value(&ScalarExpr::literal_f64(-1.0))?;
                let op = Operator::series_binary(
                    self.schema(&value),
                    self.schema(&minus),
                    kernel(crate::expressions::binary::BinaryOpKind::Arithmetic(
                        planner_types::pre_asap::ArithmeticOpKind::Mul,
                    )),
                    [true, true],
                )?;
                Ok(self.add(op, vec![value, minus]))
            }
            _ => {
                let (left, right, kind) = scalar_binary(expr)?;
                let sides = vec![self.scalar_value(left)?, self.scalar_value(right)?];
                let op = Operator::series_binary(
                    self.schema(&sides[0]),
                    self.schema(&sides[1]),
                    kernel(kind),
                    [true, true],
                )?;
                Ok(self.add(op, sides))
            }
        }
    }

    /// `function(matrix)`, where the matrix is a range selector or a subquery.
    fn range_function(
        &mut self,
        function: &AggIntent,
        matrix: &OperatorNode,
        logical: &OperatorNode,
    ) -> Result<Input, Error> {
        let function = unbound(function)?;
        let (subquery, offset, at_ms) = match matrix.expect_non_asap() {
            NonASAPOp::TimeShift { shift, child } => (child.as_ref(), shift.offset_ms, at(shift)?),
            _ => (matrix, 0, None),
        };
        let NonASAPOp::PromqlSubquery {
            range: outer,
            resolution,
            child,
        } = subquery.expect_non_asap()
        else {
            let (range, offset, at) = selector(matrix)?;
            let input = self.read(matrix)?;
            let schema = self.schema(&input);
            return self.push(
                Operator::series_window(schema, Some(function), range, offset, at, None)?
                    .with_series_range_bounds(range_anchor(matrix), None)?,
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
        let (inner, selected) = match child.expect_non_asap() {
            NonASAPOp::Aggregate {
                reduction: planner_types::pre_asap::Reduction::PerEntity,
                measures,
                having: None,
                child: selected,
                ..
            } => match measures.as_slice() {
                [inner] => (Some(unbound(inner)?), selected.as_ref()),
                _ => return Err(invalid("range function requires one measure")),
            },
            _ => (None, child.as_ref()),
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
            )?
            .with_series_range_bounds(range_anchor(selected), range_anchor(matrix))?,
            vec![raw],
            child,
        )?;
        // Name removal must validate each subquery evaluation step.
        if inner.is_some() && !matches!(inner, Some(AggIntent::LastOverTime)) {
            let input = self.schema(&step);
            let relabel = Operator::series_without_name(input)?;
            step = self.add(relabel, vec![step]);
        }
        let input = self.schema(&step);
        self.push(
            Operator::series_window(input, Some(function), steps.range_ms, offset, at_ms, None)?
                .with_series_range_bounds(range_anchor(matrix), None)?,
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
        logical: &OperatorNode,
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
            .filter(|(_, f)| f.dtype == FieldDataType::Plain(DataType::Float64))
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        let [value] = value.as_slice() else {
            return Err(invalid("PromQL aggregation requires one Float64 value"));
        };
        let value = *value;
        let reduction = match measure {
            AggIntent::Sum { .. } => Reduction::Sum(value),
            AggIntent::Avg { .. } => Reduction::Avg(value),
            AggIntent::Min { .. } => Reduction::Min(value),
            AggIntent::Max { .. } => Reduction::Max(value),
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
        AggIntent::Deriv => AggIntent::Deriv,
        AggIntent::PredictLinear { seconds } => AggIntent::PredictLinear { seconds: *seconds },
        AggIntent::Increase => AggIntent::Increase,
        AggIntent::Delta => AggIntent::Delta,
        AggIntent::Count { accuracy } => AggIntent::Count {
            accuracy: accuracy.clone(),
        },
        AggIntent::Sum { .. } => AggIntent::Sum { col: None },
        AggIntent::Avg { .. } => AggIntent::Avg { col: None },
        AggIntent::Min { .. } => AggIntent::Min { col: None },
        AggIntent::Max { .. } => AggIntent::Max { col: None },
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

fn kernel(
    kind: crate::expressions::binary::BinaryOpKind,
) -> crate::expressions::binary::BinaryOperator {
    crate::expressions::binary::BinaryOperator {
        kind,
        vector_match: None,
        checked_relative_division: false,
        checked_finite_division: false,
    }
}

fn scalar_binary(
    expr: &ScalarExpr,
) -> Result<
    (
        &ScalarExpr,
        &ScalarExpr,
        crate::expressions::binary::BinaryOpKind,
    ),
    Error,
> {
    use crate::expressions::binary::BinaryOpKind as K;
    match expr {
        ScalarExpr::Arithmetic {
            left,
            right,
            op,
            semantics: planner_types::ir::ExprSemantics::Promql,
        } => Ok((left, right, K::Arithmetic(op.clone()))),
        ScalarExpr::Compare {
            left,
            right,
            op,
            semantics: planner_types::ir::ExprSemantics::Promql,
        } => Ok((left, right, K::Compare(op.clone()))),
        ScalarExpr::Case {
            operand: None,
            branches,
            else_expr,
        } if matches!(else_expr.as_deref(), Some(ScalarExpr::Literal(planner_types::pre_asap::ScalarValue::Float64(v))) if *v == 0.0) =>
        {
            let [(
                ScalarExpr::Compare {
                    left,
                    right,
                    op,
                    semantics: planner_types::ir::ExprSemantics::Promql,
                },
                ScalarExpr::Literal(planner_types::pre_asap::ScalarValue::Float64(v)),
            )] = branches.as_slice()
            else {
                return Err(invalid("unsupported scalar case"));
            };
            if *v != 1.0 {
                return Err(invalid("unsupported scalar case result"));
            }
            Ok((left, right, K::CompareBool(op.clone())))
        }
        _ => Err(invalid("scalar expression has no native temporal lowering")),
    }
}
