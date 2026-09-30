//! Query-time PromQL value computation over logical row schemas.
use super::*;
use planner_types::post_asap::maintained_population::PopulationReadout;

/// Aggregate readouts of a maintained current-series population.
pub(super) fn population_aggregate(
    input: &Schema,
    grouping: &[String],
    readout: &PopulationReadout,
) -> Result<Operator, Error> {
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
    Operator::aggregate(input.clone(), groups, vec![("value".into(), reduction)])
}
