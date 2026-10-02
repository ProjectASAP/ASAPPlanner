//! Physical scalar/vector contracts preserve complete label sets across native computation.
use super::*;

pub fn scalar_schema() -> SchemaRef {
    crate::operators::vector_binary::value_schema(true)
}
pub fn vector_schema() -> SchemaRef {
    crate::operators::vector_binary::value_schema(false)
}

pub fn matrix_schema() -> SchemaRef {
    crate::operators::vector_window::matrix_schema()
}

pub fn compile_scalar(value: f64) -> Result<CompiledPhysicalDAG, Error> {
    let operator = Operator::scalar(
        crate::values::Value::Float64(value),
        planner_types::pre_asap::DataType::Float64,
    )?
    .with_output_schema(scalar_schema())?;
    CompiledPhysicalDAG::from_operators(
        BTreeMap::new(),
        BTreeMap::from([(0, (vec![], operator))]),
        vec![0],
    )
}

pub fn compile_temporal(
    intent: &AggIntent<ColumnRef>,
    preserve_metric_name: bool,
) -> Result<CompiledPhysicalDAG, Error> {
    let operator = Operator::range_window(intent.clone())?;
    let mut operators = vec![operator];
    if !preserve_metric_name {
        operators.push(Operator::project(
            vector_schema(),
            vec![
                (
                    "labels".into(),
                    Expression::LabelSet {
                        column: 0,
                        labels: vec![],
                        without: true,
                    },
                ),
                ("value".into(), Expression::Column(1)),
            ],
        )?);
    }
    unary(operators, matrix_schema())
}

pub fn compile_histogram_quantile() -> Result<CompiledPhysicalDAG, Error> {
    CompiledPhysicalDAG::from_operators(
        BTreeMap::from([
            (0, InputContract::bounded(scalar_schema())),
            (1, InputContract::bounded(vector_schema())),
        ]),
        BTreeMap::from([(2, (vec![0, 1], Operator::histogram_quantile()))]),
        vec![2],
    )
}

/// Compile before deployment chooses readers. Input slots 0 and 1 retain operand order.
pub fn compile_binary(
    operator: &planner_types::post_asap::BinaryOperator,
    return_bool: bool,
    left_scalar: bool,
    right_scalar: bool,
) -> Result<CompiledPhysicalDAG, Error> {
    let left = crate::operators::vector_binary::value_schema(left_scalar);
    let right = crate::operators::vector_binary::value_schema(right_scalar);
    let op = Operator::vector_binary(left.clone(), right.clone(), operator.clone(), return_bool)?;
    CompiledPhysicalDAG::from_operators(
        BTreeMap::from([
            (0, InputContract::bounded(left)),
            (1, InputContract::bounded(right)),
        ]),
        BTreeMap::from([(2, (vec![0, 1], op))]),
        vec![2],
    )
}

fn unary(operators: Vec<Operator>, input: SchemaRef) -> Result<CompiledPhysicalDAG, Error> {
    let root = operators.len() as u64;
    CompiledPhysicalDAG::from_operators(
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

fn vector_output(input: SchemaRef, labels: usize, value: usize) -> Result<Operator, Error> {
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
) -> Result<CompiledPhysicalDAG, Error> {
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
) -> Result<CompiledPhysicalDAG, Error> {
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
) -> Result<CompiledPhysicalDAG, Error> {
    let project = grouped(grouping)?;
    let limit = Operator::limit(project.schema(), n, offset, vec![2])?;
    let output = vector_output(limit.schema(), 0, 1)?;
    unary(vec![project, limit, output], vector_schema())
}

pub fn compile_negate(scalar: bool) -> Result<CompiledPhysicalDAG, Error> {
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

pub fn compile_vector_to_scalar() -> Result<CompiledPhysicalDAG, Error> {
    unary(
        vec![Operator::vector_to_scalar(vector_schema(), 1)?.with_output_schema(scalar_schema())?],
        vector_schema(),
    )
}

/// A stored exact-state input retains the complete population identity. The
/// deployment supplies eligible panes; merging and finalization are computation.
pub fn exact_state_schema(family: FieldDataType) -> Result<SchemaRef, Error> {
    if !matches!(family, FieldDataType::ExactAggregate(..)) {
        return Err(invalid("exact-state input requires an exact family"));
    }
    crate::values::validate_family(&family)?;
    let mut schema = (*vector_schema()).clone();
    schema.fields[1].dtype = family;
    Ok(Arc::new(schema))
}

/// Retain exact readout semantics before any deployment state is opened.
pub fn compile_exact_readout(
    family: FieldDataType,
    lookback_ms: u64,
    preserve_metric_name: bool,
) -> Result<CompiledPhysicalDAG, Error> {
    use planner_types::post_asap::ExactKind;
    let statistic = match &family {
        FieldDataType::ExactAggregate(kind, _) => match kind {
            ExactKind::Sum => crate::Statistic::Sum,
            ExactKind::Count => crate::Statistic::Count,
            ExactKind::Min => crate::Statistic::Min,
            ExactKind::Max => crate::Statistic::Max,
            ExactKind::Rate => crate::Statistic::Rate,
            ExactKind::Increase => crate::Statistic::Increase,
            ExactKind::IRate => return Err(invalid("instant-rate state readout is not supported")),
        },
        _ => return Err(invalid("exact readout requires an exact family")),
    };
    let input = exact_state_schema(family)?;
    let merge = Operator::summary_merge(input.clone(), 1, vec![0])?;
    let mut readout = Operator::readout(
        merge.schema(),
        1,
        ReadoutQuery::Exact(ExactReadout {
            statistic,
            lookback_ms: None,
        }),
    )?;
    if matches!(
        statistic,
        crate::Statistic::Rate | crate::Statistic::Increase
    ) {
        readout = readout.with_counter_lookback(
            i64::try_from(lookback_ms).map_err(|_| invalid("counter lookback exceeds Int64"))?,
        )?;
    }
    let project = Operator::project(
        readout.schema(),
        vec![
            (
                "labels".into(),
                if preserve_metric_name {
                    Expression::Column(0)
                } else {
                    Expression::LabelSet {
                        column: 0,
                        labels: vec![],
                        without: true,
                    }
                },
            ),
            ("value".into(), Expression::ExactFloat64(1)),
        ],
    )?;
    unary(vec![merge, readout, project], input)
}
