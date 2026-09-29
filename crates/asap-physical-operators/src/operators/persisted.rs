//! Recovery validates operator contracts, without selecting or lowering a plan.
use super::*;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StoredOperator {
    kind: Kind,
    inputs: Vec<Schema>,
    output: Schema,
}
impl TryFrom<StoredOperator> for Operator {
    type Error = Error;
    fn try_from(stored: StoredOperator) -> Result<Self, Error> {
        let expected_kind =
            serde_json::to_value(&stored.kind).map_err(|error| invalid(&error.to_string()))?;
        let StoredOperator {
            kind,
            inputs,
            output,
        } = stored;
        for schema in inputs.iter().chain(std::iter::once(&output)) {
            crate::values::validate_schema(schema)?;
        }
        let input = |index| {
            inputs
                .get(index)
                .cloned()
                .ok_or_else(|| invalid("missing persisted input"))
        };
        let op = match kind {
            Kind::Source(_) => return Err(invalid("physical plans cannot persist live sources")),
            Kind::PaneInput {
                coordinate,
                layout,
                offset_ms,
            } => Operator::pane_input(input(0)?, coordinate, layout, offset_ms)?,
            Kind::ScopeTimestamp { .. } => Operator::scope_timestamp(input(0)?, output.clone())?,
            Kind::Union => Operator::union(input(0)?, inputs.len())?,
            Kind::CurrentSeries {
                identity,
                coordinate,
                value,
                lookback_ms,
            } => Operator::current_series(input(0)?, identity, coordinate, value, lookback_ms)?,
            Kind::VectorToScalar { column } => Operator::vector_to_scalar(input(0)?, column)?,
            Kind::VectorBinary {
                operator,
                return_bool,
            } => Operator::vector_binary(input(0)?, input(1)?, operator, return_bool)?,
            Kind::Project(expressions) => {
                if expressions.len() != output.fields.len() {
                    return Err(invalid("persisted projection width mismatch"));
                }
                Operator::project(
                    input(0)?,
                    output
                        .fields
                        .iter()
                        .zip(expressions)
                        .map(|(f, e)| (f.name.clone(), e))
                        .collect(),
                )?
            }
            Kind::Filter(expression) => Operator::filter(input(0)?, expression)?,
            Kind::Limit { n, offset, groups } => Operator::limit(input(0)?, n, offset, groups)?,
            Kind::Sort { keys, groups } => Operator::sort(input(0)?, keys, groups)?,
            Kind::Window {
                intent,
                coordinate,
                value,
                groups,
                window,
            } => Operator::window(input(0)?, *intent, coordinate, value, groups, window)?,
            Kind::Aggregate { groups, measures } => {
                if groups.len() + measures.len() != output.fields.len() {
                    return Err(invalid("persisted aggregate width mismatch"));
                }
                let names = output.fields[groups.len()..].iter().map(|f| f.name.clone());
                Operator::aggregate(input(0)?, groups, names.zip(measures).collect())?
            }
            Kind::SemiJoin { keys } => Operator::semi_join(input(0)?, input(1)?, keys)?,
            Kind::Join { kind, predicate } => Operator::relational_join(
                input(0)?,
                input(1)?,
                kind,
                &planner_types::pre_asap::Predicate(std::rc::Rc::new(
                    predicate.expression().clone(),
                )),
                output.clone(),
            )?,
            Kind::SummaryBuild {
                family,
                value,
                time,
                groups,
            } => Operator::summary_build(input(0)?, family, value, time, groups)?,
            Kind::KeyedSummaryBuild {
                family,
                value,
                items,
                groups,
            } => Operator::keyed_summary_build(input(0)?, family, value, items, groups)?,
            Kind::KeyedReadout { state, k } => {
                Operator::keyed_readout(input(0)?, state, k, output.clone())?
            }
            Kind::SummaryMerge { state, groups } => {
                Operator::summary_merge(input(0)?, state, groups)?
            }
            Kind::Readout {
                state,
                statistic,
                parameters,
            } => Operator::readout(input(0)?, state, statistic, parameters)?,
        }
        .with_output_schema(output)?;
        if serde_json::to_value(&op.kind).map_err(|error| invalid(&error.to_string()))?
            != expected_kind
        {
            return Err(invalid(
                "persisted operator contains inconsistent compiled fields",
            ));
        }
        if op.inputs != inputs {
            return Err(invalid("persisted operator input contracts differ"));
        }
        Ok(op)
    }
}
