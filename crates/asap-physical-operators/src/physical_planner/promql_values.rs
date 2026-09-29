//! Physical scalar/vector contracts preserve complete label sets across native computation.
use super::*;

pub fn scalar_schema() -> Schema {
    crate::operators::vector_binary::value_schema(true)
}
pub fn vector_schema() -> Schema {
    crate::operators::vector_binary::value_schema(false)
}

/// Compile before deployment chooses readers. Input slots 0 and 1 retain operand order.
pub fn compile_binary(
    operator: &planner_types::post_asap::BinaryOperator,
    return_bool: bool,
    left_scalar: bool,
    right_scalar: bool,
) -> Result<CompiledPhysicalDag, Error> {
    let left = crate::operators::vector_binary::value_schema(left_scalar);
    let right = crate::operators::vector_binary::value_schema(right_scalar);
    let op = Operator::vector_binary(left.clone(), right.clone(), operator.clone(), return_bool)?;
    CompiledPhysicalDag::from_operators(
        BTreeMap::from([
            (0, InputContract::bounded(left)),
            (1, InputContract::bounded(right)),
        ]),
        BTreeMap::from([(2, (vec![0, 1], op))]),
        vec![2],
    )
}

fn unary(operators: Vec<Operator>, input: Schema) -> Result<CompiledPhysicalDag, Error> {
    let root = operators.len() as u64;
    CompiledPhysicalDag::from_operators(
        BTreeMap::from([(0, InputContract::bounded(input))]),
        operators
            .into_iter()
            .enumerate()
            .map(|(i, op)| ((i + 1) as u64, (vec![i as u64], op)))
            .collect(),
        vec![root],
    )
}

fn grouped(grouping: &GroupKeys<ColumnRef>) -> Result<Operator, Error> {
    let labels = grouping
        .keys()
        .iter()
        .map(|key| match key {
            ColumnRef::Named(label) => Ok(label.clone()),
            _ => Err(invalid("vector grouping requires label names")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Operator::project(
        vector_schema(),
        vec![
            ("labels".into(), Expression::Column(0)),
            ("value".into(), Expression::Column(1)),
            (
                "group".into(),
                Expression::LabelSet {
                    column: 0,
                    labels,
                    without: grouping.is_without(),
                },
            ),
        ],
    )
}

fn vector_output(input: Schema, labels: usize, value: usize) -> Result<Operator, Error> {
    let value = Expression::ExactFloat64(value);
    Operator::project(
        input,
        vec![
            ("labels".into(), Expression::Column(labels)),
            ("value".into(), value),
        ],
    )
}

pub fn compile_aggregate(
    intent: &AggIntent<ColumnRef>,
    grouping: &GroupKeys<ColumnRef>,
) -> Result<CompiledPhysicalDag, Error> {
    let project = grouped(grouping)?;
    let reduction = match intent {
        AggIntent::Sum { .. } => Reduction::Sum(1),
        AggIntent::Avg { .. } => Reduction::Avg(1),
        AggIntent::Count { .. } => Reduction::Count,
        AggIntent::Min { .. } => Reduction::Min(1),
        AggIntent::Max { .. } => Reduction::Max(1),
        _ => return Err(invalid("unsupported vector aggregate")),
    };
    let aggregate =
        Operator::aggregate(project.schema(), vec![2], vec![("value".into(), reduction)])?;
    let output = vector_output(aggregate.schema(), 0, 1)?;
    unary(vec![project, aggregate, output], vector_schema())
}

pub fn compile_sort(
    descending: bool,
    grouping: &GroupKeys<ColumnRef>,
) -> Result<CompiledPhysicalDag, Error> {
    let project = grouped(grouping)?;
    let sort = Operator::sort(
        project.schema(),
        vec![SortKey {
            column: 1,
            descending,
            nulls_first: false,
        }],
        vec![2],
    )?;
    let output = vector_output(sort.schema(), 0, 1)?;
    unary(vec![project, sort, output], vector_schema())
}

pub fn compile_limit(
    n: u64,
    offset: u64,
    grouping: &GroupKeys<ColumnRef>,
) -> Result<CompiledPhysicalDag, Error> {
    let project = grouped(grouping)?;
    let limit = Operator::limit(project.schema(), n, offset, vec![2])?;
    let output = vector_output(limit.schema(), 0, 1)?;
    unary(vec![project, limit, output], vector_schema())
}

pub fn compile_negate(scalar: bool) -> Result<CompiledPhysicalDag, Error> {
    let input = if scalar {
        scalar_schema()
    } else {
        vector_schema()
    };
    let mut columns = Vec::new();
    if !scalar {
        columns.push(("labels".into(), Expression::Column(0)));
    }
    columns.push((
        if scalar {
            "$promql_scalar".into()
        } else {
            "value".into()
        },
        Expression::Negate(Box::new(Expression::Column(if scalar { 0 } else { 1 }))),
    ));
    unary(vec![Operator::project(input.clone(), columns)?], input)
}

pub fn compile_vector_to_scalar() -> Result<CompiledPhysicalDag, Error> {
    unary(
        vec![Operator::vector_to_scalar(vector_schema(), 1)?.with_output_schema(scalar_schema())?],
        vector_schema(),
    )
}
