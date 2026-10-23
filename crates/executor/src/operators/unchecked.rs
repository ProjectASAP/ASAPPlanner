//! Deserialized operators are validated before use, whatever the encoding.
use super::*;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UncheckedOperator {
    kind: Kind,
    inputs: Vec<SchemaRef>,
    output: SchemaRef,
}
impl TryFrom<UncheckedOperator> for Operator {
    type Error = Error;
    fn try_from(unchecked: UncheckedOperator) -> Result<Self, Error> {
        let expected_kind =
            serde_json::to_value(&unchecked.kind).map_err(|error| invalid(&error.to_string()))?;
        let UncheckedOperator {
            kind,
            inputs,
            output,
        } = unchecked;
        for schema in inputs.iter().chain(std::iter::once(&output)) {
            crate::values::validate_schema(schema)?;
        }
        let input = |index| {
            inputs
                .get(index)
                .cloned()
                .ok_or_else(|| invalid("missing operator input"))
        };
        let op = match kind {
            Kind::Source(_) => return Err(invalid("physical plans cannot serialize live sources")),
            Kind::EvaluationTime => Operator::evaluation_time(),
            Kind::Constant { value, dtype } => Operator::scalar(value, dtype)?,
            Kind::ScopeTimestamp { .. } => Operator::scope_timestamp(input(0)?, output.clone())?,
            Kind::Union => Operator::union(input(0)?, inputs.len())?,
            Kind::CurrentSeries {
                identity,
                coordinate,
                value,
                lookback_ms,
            } => Operator::current_series(input(0)?, identity, coordinate, value, lookback_ms)?,
            Kind::VectorToScalar { column } => Operator::vector_to_scalar(input(0)?, column)?,
            Kind::Absent { labels } => Operator::absent(input(0)?, labels, output.clone())?,
            Kind::VectorBinary {
                operator,
                return_bool,
            } => Operator::vector_binary(input(0)?, input(1)?, operator, return_bool)?,
            Kind::AlignedBinary {
                keys,
                values,
                operator,
            } => Operator::aligned_binary(input(0)?, input(1)?, keys, values, operator)?,
            Kind::RangeWindow { intent } => Operator::range_window(*intent)?,
            Kind::HistogramQuantile => Operator::histogram_quantile(),
            Kind::SeriesWindow {
                function,
                range_ms,
                offset_ms,
                at_ms,
                steps,
                range_at,
                steps_range_at,
                ..
            } => Operator::series_window(
                input(0)?,
                function.map(|f| *f),
                range_ms,
                offset_ms,
                at_ms,
                steps,
            )?
            .with_series_range_bounds(range_at, steps_range_at)?,
            // The rebuilt kind must equal the serialized one, which rejects
            // a unique rewrite with other matching labels.
            Kind::SeriesLabels { unique: true, .. } => Operator::series_without_name(input(0)?)?,
            Kind::SeriesLabels { kind, labels, .. } => {
                Operator::series_labels(input(0)?, kind, labels)?
            }
            Kind::SeriesBinary { operator, scalars } => {
                Operator::series_binary(input(0)?, input(1)?, operator, scalars)?
            }
            Kind::SeriesRelabel {
                destination,
                replacement,
                source_regex,
            } => Operator::series_relabel(
                input(0)?,
                output.clone(),
                destination,
                replacement,
                source_regex,
            )?,
            Kind::SeriesHistogramQuantile { quantile, le } => {
                Operator::series_histogram_quantile(input(0)?, f64::from_bits(quantile), le)?
            }
            Kind::Project(expressions) => {
                if expressions.len() != output.fields.len() {
                    return Err(invalid("projection width mismatch"));
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
            Kind::Aggregate {
                groups,
                measures,
                filters,
            } => {
                if groups.len() + measures.len() != output.fields.len() {
                    return Err(invalid("aggregate width mismatch"));
                }
                let names = output.fields[groups.len()..].iter().map(|f| f.name.clone());
                Operator::aggregate(input(0)?, groups, names.zip(measures).collect())?
                    .with_measure_filters(filters)?
            }
            Kind::SemiJoin {
                keys,
                require_complete_right,
            } => {
                let operator = Operator::semi_join(input(0)?, input(1)?, keys)?;
                if require_complete_right {
                    operator.require_complete_right()
                } else {
                    operator
                }
            }
            Kind::Join { kind, predicate } => Operator::relational_join(
                input(0)?,
                input(1)?,
                kind,
                // A compiled predicate reads no other operator.
                &planner_types::ir::Predicate(predicate.expression().map_operator_refs(
                    &mut |_| -> std::rc::Rc<planner_types::ir::OperatorNode> {
                        unreachable!("compiled expressions hold no operator reference")
                    },
                )),
                output.clone(),
            )?,
            Kind::PerGroupSummaryBuild {
                family,
                value,
                time,
                groups,
                filter,
            } => with_filter(
                Operator::per_group_summary_build(input(0)?, family, value, time, groups)?,
                filter,
            )?,
            Kind::PerGroupKeyedSummaryBuild {
                family,
                items,
                row_weight,
                groups,
                filter,
            } => with_filter(
                Operator::per_group_keyed_summary_build(
                    input(0)?,
                    family,
                    items,
                    row_weight,
                    groups,
                )?,
                filter,
            )?,
            Kind::AllGroupsSummaryBuild {
                family,
                item,
                row_weight,
                groups,
                filter,
            } => with_filter(
                Operator::all_groups_summary_build(input(0)?, family, item, row_weight, groups)?,
                filter,
            )?,
            Kind::KeyedEvaluation { state, k } => {
                Operator::keyed_evaluation(input(0)?, state, k, output.clone())?
            }
            Kind::SummaryMerge {
                state,
                groups,
                emit_empty_global,
            } => {
                let merge = Operator::summary_merge(input(0)?, state, groups)?;
                if emit_empty_global {
                    merge.with_emit_empty_global()?
                } else {
                    merge
                }
            }
            Kind::Evaluation { state, query } => Operator::evaluation(input(0)?, state, query)?,
        }
        .with_output_schema(output)?;
        if serde_json::to_value(&op.kind).map_err(|error| invalid(&error.to_string()))?
            != expected_kind
        {
            return Err(invalid("operator contains inconsistent compiled fields"));
        }
        if op.inputs != inputs {
            return Err(invalid("operator input contracts differ"));
        }
        Ok(op)
    }
}

fn with_filter(operator: Operator, filter: Option<Box<Expression>>) -> Result<Operator, Error> {
    match filter {
        Some(filter) => operator.with_row_filter(*filter),
        None => Ok(operator),
    }
}
