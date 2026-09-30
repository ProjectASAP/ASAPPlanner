//! Query-time PromQL value computation over logical row schemas.
use super::*;
use planner_types::post_asap::maintained_population::PopulationReadout;
use planner_types::pre_asap::{DataType, ScalarValue};

/// A PromQL number literal has no row schema; its consumer folds it in.
pub(super) fn scalar_literal(expression: &PreASAPNode) -> Option<f64> {
    match expression {
        PreASAPNode::PromqlScalarBridge(child) => scalar_literal(child),
        PreASAPNode::Literal(ScalarValue::Float64(value)) => Some(*value),
        _ => None,
    }
}

/// Aggregate readouts of a maintained current-series population, as a chain.
pub(super) fn population_aggregate(
    input: &Schema,
    grouping: &[String],
    readout: &PopulationReadout,
) -> Result<Vec<Operator>, Error> {
    let groups = grouping
        .iter()
        .map(|name| named_column(input, &ColumnRef::Named(name.clone())))
        .collect::<Result<Vec<_>, _>>()?;
    let value = named_column(input, &ColumnRef::SampleValue)?;
    let reduction = match readout {
        PopulationReadout::Sum => Reduction::Sum(value),
        PopulationReadout::Count => Reduction::Count,
        PopulationReadout::Average => Reduction::Avg(value),
        PopulationReadout::Quantile { q } => Reduction::Quantile {
            column: value,
            q: *q,
        },
        PopulationReadout::TopK { .. } => {
            return Err(invalid(
                "TopK population readout ranks; it does not aggregate",
            ))
        }
    };
    if !groups.is_empty() {
        return Ok(vec![Operator::aggregate(
            input.clone(),
            groups,
            vec![("value".into(), reduction)],
        )?]);
    }
    // A global aggregate over no members is an empty PromQL vector, not one row.
    let aggregate = Operator::aggregate(
        input.clone(),
        vec![],
        vec![
            ("value".into(), reduction),
            ("members".into(), Reduction::Count),
        ],
    )?;
    let zero = Expression::Literal {
        value: crate::values::Value::Int64(0),
        dtype: DataType::Int64,
    };
    let filter = Operator::filter(
        aggregate.schema(),
        Expression::Less(Box::new(zero), Box::new(Expression::Column(1))),
    )?;
    let project = Operator::project(
        filter.schema(),
        vec![("value".into(), Expression::Column(0))],
    )?;
    Ok(vec![aggregate, filter, project])
}
