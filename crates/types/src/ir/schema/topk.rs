//! Ranked-row output contract shared by logical TopK and its implementations.
use crate::ir::operator::input::{scans_only_time_series, OperatorInput};
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
pub fn ranked_column<I: OperatorInput>(child: &I) -> Result<ColumnId, ResolveError> {
    let mut node = child.clone();
    let reduction = loop {
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
            Operator::ASAP(ASAPOp::SummaryAgg { reduction, .. }) => break Some(reduction),
            Operator::NonASAP(NonASAPOp::Aggregate {
                reduction,
                measures,
                ..
            }) if measures.len() == 1 => {
                if matches!(measures[0], crate::ir::operator::AggIntent::TopK { .. }) {
                    return Ok(reduction.group_keys().map_or(0, |keys| keys.len()));
                }
                break Some(reduction);
            }
            _ => break None,
        }
    };
    match reduction {
        Some(Reduction::Reduce(_)) => Ok(child.schema().fields.len() - 1),
        _ => resolve_column_ref(&ColumnRef::SampleValue, child.schema()),
    }
}

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
    let promql = scans_only_time_series(&child.scanned_sources());
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
