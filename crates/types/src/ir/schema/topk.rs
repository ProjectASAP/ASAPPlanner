//! Ranked-row output contract shared by logical TopK and its implementations.
use crate::ir::operator::arguments::Source;
use crate::ir::operator::input::OperatorInput;
use crate::ir::operator::Reduction;
use crate::ir::scalar::column_resolution::{resolve_column_ref, ResolveError};
use crate::ir::scalar::{ColumnRef, ProjectItem};
use crate::ir::schema::{ColumnId, DataType, FieldDataType, PROMQL_SERIES_IDENTITY};
use crate::ir::{ASAPOp, NonASAPOp, Operator, ScalarExpr};

/// The column a top-k ranks `child` by. Over a one-measure grouped
/// aggregate (the SQL heavy-hitter shape and PromQL `topk(k, sum by ..)`) it
/// is that measure, the last column after the group keys, whichever Stage 1
/// candidate realizes it: the aggregate itself, or a `SummaryAgg` read by
/// `FinalizeExactAccumulator` or `SummaryEstimate`, under the `Project`
/// restoring its names (a renaming) and a value `HAVING`'s `Filter`. Over
/// per-series rows (PromQL `topk by (..) (k, rows)`) it is the sample value.
pub(crate) fn ranked_column<I: OperatorInput>(child: &I) -> Result<ColumnId, ResolveError> {
    match ranked_source(child) {
        RankedSource::TopK(reduction) => Ok(reduction.group_keys().map_or(0, |keys| keys.len())),
        RankedSource::OneRowPerItem(Reduction::Reduce(_)) => Ok(child.schema().fields.len() - 1),
        RankedSource::OneRowPerItem(Reduction::PerEntity) | RankedSource::Rows => {
            resolve_column_ref(&ColumnRef::SampleValue, child.schema())
        }
    }
}

/// What a top-k's input rows are, seen through a renaming `Project`, a
/// value `HAVING`'s `Filter`, a finalize or a summary readout.
enum RankedSource {
    /// Another top-k's ranked rows, grouped by this reduction.
    TopK(Reduction),
    /// One row per item: a one-measure aggregate (or a summary build), per
    /// group of this reduction or per series.
    OneRowPerItem(Reduction),
    /// Any other rows, possibly several per item.
    Rows,
}

fn ranked_source<I: OperatorInput>(child: &I) -> RankedSource {
    let mut node = child.clone();
    loop {
        match node.operator() {
            // Only a renaming keeps the measure last.
            Operator::NonASAP(NonASAPOp::Project { cols, child, .. })
                if cols.len() == child.schema().fields.len()
                    && cols
                        .iter()
                        .enumerate()
                        .all(|(i, item)| matches!(item.expr, ScalarExpr::Column(c) if c == i)) =>
            {
                node = child
            }
            Operator::NonASAP(NonASAPOp::Filter { child, .. })
            | Operator::ASAP(
                ASAPOp::FinalizeExactAccumulator { child }
                | ASAPOp::SummaryEstimate {
                    summary_input: child,
                    ..
                },
            ) => node = child,
            Operator::ASAP(ASAPOp::SummaryAgg { reduction, .. }) => {
                return RankedSource::OneRowPerItem(reduction)
            }
            Operator::NonASAP(NonASAPOp::Aggregate {
                reduction,
                measures,
                ..
            }) if measures.len() == 1 => {
                if matches!(measures[0], crate::ir::operator::AggIntent::TopK { .. }) {
                    return RankedSource::TopK(reduction);
                }
                return RankedSource::OneRowPerItem(reduction);
            }
            _ => return RankedSource::Rows,
        }
    }
}

/// Whether the item columns `item_columns` of `input` identify each item
/// completely: `input` lists every column its rows carry (it is closed), or
/// the items include the realized PromQL series identity. An open PromQL
/// label set may carry unlisted labels, so its listed columns cannot.
pub(crate) fn item_identity_is_complete(
    input: &crate::ir::schema::Schema,
    item_columns: impl IntoIterator<Item = ColumnId>,
) -> bool {
    input.closed
        || item_columns
            .into_iter()
            .any(|index| input.fields[index].name == PROMQL_SERIES_IDENTITY)
}

/// The unique key of ranked rows (one row per partition and item) with
/// `columns` columns, the ranked value at `value`: every other column. The
/// one rule for a top-k's ranked rows, exact or a heap's readout.
pub(crate) fn ranked_rows_unique_keys(columns: usize, value: ColumnId) -> Vec<Vec<ColumnId>> {
    let key: Vec<ColumnId> = (0..columns).filter(|&column| column != value).collect();
    if key.is_empty() {
        Vec::new()
    } else {
        vec![key]
    }
}

/// The unique keys of an exact top-k's ranked rows (`output`, the
/// projection [`ranked_row_projection`] gives, its ranked value after the
/// `partitions` keys) over `child`: one row per (partition, item) when
/// `child` has one row per item whose identity is complete
/// ([`item_identity_is_complete`]); else the projection's own keys.
pub(crate) fn exact_topk_unique_keys<I: OperatorInput>(
    child: &I,
    partitions: usize,
    output: &crate::ir::schema::Schema,
) -> Vec<Vec<ColumnId>> {
    let item_columns = (0..child.schema().fields.len()).filter(|&index| {
        output
            .fields
            .iter()
            .skip(partitions + 1)
            .any(|field| field.name == child.schema().fields[index].name)
    });
    if matches!(ranked_source(child), RankedSource::OneRowPerItem(_))
        && item_identity_is_complete(child.schema(), item_columns)
    {
        ranked_rows_unique_keys(output.fields.len(), partitions)
    } else {
        output.unique_keys.clone()
    }
}

/// The name of a top-k aggregate's ranked value column: the aggregate's
/// first output name if it gives a non-empty one, else `topk_{k}`.
pub(crate) fn topk_output_name(k: usize, output_names: &[String]) -> String {
    output_names
        .first()
        .filter(|name| !name.is_empty())
        .cloned()
        .unwrap_or_else(|| format!("topk_{k}"))
}

/// Ordered projection of ranked rows: partition keys, ranked value, then
/// item identity. TopK retains rows; partition keys alone are not unique.
pub(crate) fn ranked_row_projection<I: OperatorInput>(
    child: &I,
    reduction: &Reduction,
    output_name: &str,
) -> Result<Vec<ProjectItem<I>>, crate::ir::SchemaDerivationError> {
    use crate::ir::SchemaDerivationError;
    let value = ranked_column(child)
        .map_err(|error| SchemaDerivationError::InvalidScalarSignature(error.to_string()))?;
    let keys = reduction.group_keys().map_or(&[][..], |keys| keys.keys());
    for &index in keys {
        if index >= child.schema().fields.len() {
            return Err(SchemaDerivationError::InvalidGroupByColumn(
                index,
                child.schema().fields.len(),
            ));
        }
    }
    let mut columns: Vec<ProjectItem<I>> = keys
        .iter()
        .map(|&index| ProjectItem {
            alias: Some(child.schema().fields[index].name.clone()),
            expr: ScalarExpr::Column(index),
        })
        .collect();
    columns.push(ProjectItem {
        alias: Some(output_name.into()),
        expr: ScalarExpr::Column(value),
    });
    let promql = child.scanned_sources().iter().all(Source::is_time_series);
    for (index, field) in child.schema().fields.iter().enumerate() {
        let names_item = !promql || field.dtype == FieldDataType::Plain(DataType::Utf8);
        if names_item
            && index != value
            && !keys.contains(&index)
            && field.name != PROMQL_SERIES_IDENTITY
        {
            columns.push(ProjectItem {
                alias: Some(field.name.clone()),
                expr: ScalarExpr::Column(index),
            });
        }
    }
    // Identity realization appends a column; keep every existing label in place
    // so consumers' already-bound column positions remain valid.
    if let Ok(identity) = resolve_column_ref(
        &ColumnRef::Named(PROMQL_SERIES_IDENTITY.into()),
        child.schema(),
    ) {
        if !keys.contains(&identity) {
            columns.push(ProjectItem {
                alias: Some(PROMQL_SERIES_IDENTITY.into()),
                expr: ScalarExpr::Column(identity),
            });
        }
    }
    Ok(columns)
}

/// Keep named labels alongside the opaque identity in a heap item, so its
/// ranked-row readout can reproduce the same columns as exact TopK.
pub fn series_identity_item(
    schema: &crate::ir::schema::Schema,
    partitions: &[ColumnId],
) -> crate::summary_type::SummaryInputExpr {
    use crate::summary_type::SummaryInputExpr;
    let mut items: Vec<_> = schema
        .fields
        .iter()
        .enumerate()
        .filter(|(index, field)| {
            !partitions.contains(index)
                && field.dtype == FieldDataType::Plain(DataType::Utf8)
                && field.name != PROMQL_SERIES_IDENTITY
        })
        .map(|(_, field)| SummaryInputExpr::Column(field.column_ref()))
        .collect();
    items.push(SummaryInputExpr::Column(ColumnRef::Named(
        PROMQL_SERIES_IDENTITY.into(),
    )));
    if items.len() == 1 {
        items.remove(0)
    } else {
        SummaryInputExpr::Tuple(items)
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;
    use std::time::Duration;

    use crate::ir::operator::arguments::{Reduction, Source};
    use crate::ir::operator::AggIntent;
    use crate::ir::scalar::ColumnRef;
    use crate::ir::schema::series_identity::with_promql_series_identity;
    use crate::ir::schema::{DataType, Field, FieldDataType, Schema};
    use crate::ir::{
        ASAPOp, NonASAPOp, Operator, OperatorNode, ProjectItem, ScalarExpr, TimeRangeKind,
    };
    use crate::summary_type::{SketchAlgorithm, SketchStatistic, SummaryUpdate, WeightDomain};

    fn node(operator: Operator) -> Rc<OperatorNode> {
        OperatorNode::new_shared(operator).unwrap()
    }

    /// `topk by (job) (10, sum_over_time(m[1m]))` over series whose identity
    /// is realized: the exact top-k and a heap's ranked readout of the same
    /// rows (a `Project` naming the estimate's columns as the top-k's)
    /// derive the same unique keys, one row per (job, series).
    #[test]
    fn exact_topk_and_heap_readout_derive_the_same_unique_keys() {
        let open = Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("value", DataType::Float64, false),
                Field::plain("job", DataType::Utf8, true),
            ],
            0,
            vec![],
        );
        let rows = with_promql_series_identity(&node(Operator::NonASAP(NonASAPOp::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: open,
        })))
        .unwrap();
        let window = node(Operator::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_secs(60),
            kind: TimeRangeKind::Range,
            child: rows,
        }));
        let per_series = node(Operator::NonASAP(NonASAPOp::Aggregate {
            child: Rc::clone(&window),
            reduction: Reduction::PerEntity,
            measures: vec![AggIntent::Sum { col: None }],
            output_names: vec![String::new()],
            filters: vec![None],
            having: None,
        }));
        let exact = node(Operator::NonASAP(NonASAPOp::Aggregate {
            child: per_series,
            reduction: Reduction::by(vec![2]),
            measures: vec![AggIntent::TopK { k: 10 }],
            output_names: vec![String::new()],
            filters: vec![None],
            having: None,
        }));
        let heap = node(Operator::ASAP(ASAPOp::SummaryAgg {
            summary_type: FieldDataType::Sketch(SketchAlgorithm::CountSketchWithHeap),
            input: SummaryUpdate {
                item: Some(super::series_identity_item(&window.schema, &[2])),
                weight: crate::summary_type::SummaryInputExpr::Column(ColumnRef::SampleValue),
                weight_domain: WeightDomain::UnknownOrSigned,
            },
            reduction: Reduction::by(vec![2]),
            filter: None,
            child: window,
        }));
        let estimate = node(Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input: heap,
            query: SketchStatistic::TopK { k: 10 },
        }));
        let estimated = estimate.schema.fields.len() - 1;
        let cols = exact
            .schema
            .fields
            .iter()
            .enumerate()
            .map(|(index, field)| ProjectItem {
                alias: Some(field.name.clone()),
                expr: ScalarExpr::Column(if index == 1 {
                    estimated
                } else {
                    estimate
                        .schema
                        .fields
                        .iter()
                        .position(|input| input.name == field.name)
                        .unwrap()
                }),
            })
            .collect();
        let readout = node(Operator::NonASAP(NonASAPOp::Project {
            cols,
            child: estimate,
        }));
        assert!(!exact.schema.unique_keys.is_empty());
        assert_eq!(exact.schema.unique_keys, readout.schema.unique_keys);
    }
}
