//! Ranked-row output contract shared by logical TopK and its implementations.
use crate::ir::operator::{Reduction, Source};
use crate::ir::scalar::column_resolution::{resolve_column_ref, ResolveError};
use crate::ir::scalar::{ColumnRef, ProjectItem};
use crate::ir::schema::{ColumnId, DataType, FieldDataType, PROMQL_SERIES_IDENTITY};
use crate::ir::operator::AggIntent;
use crate::ir::SortKey;
use crate::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, ScalarExpr};
use std::rc::Rc;

/// The column a top-k ranks `child` by. Over a one-measure grouped
/// aggregate (the SQL heavy-hitter shape and PromQL `topk(k, sum by ..)`) it
/// is that measure, the last column after the group keys, whichever Stage 1
/// candidate realizes it: the aggregate itself, or a `SummaryAgg` read by
/// `FinalizeExactAccumulator` or `SummaryEstimate`, under the `Project`
/// restoring its names (a renaming) and a value `HAVING`'s `Filter`. Over
/// per-series rows (PromQL `topk by (..) (k, rows)`) it is the sample value.
pub fn ranked_column(child: &OperatorNode) -> Result<ColumnId, ResolveError> {
    let mut node = child;
    let reduction = loop {
        match &node.operator {
            // Only a renaming keeps the measure last.
            Operator::NonASAP(NonASAPOp::Project { cols, child, .. })
                if cols.len() == child.schema.fields.len()
                    && cols
                        .iter()
                        .enumerate()
                        .all(|(i, item)| item.expr == ScalarExpr::Column(i)) =>
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
        Some(Reduction::Reduce(_)) => Ok(child.schema.fields.len() - 1),
        _ => resolve_column_ref(&ColumnRef::SampleValue, &child.schema),
    }
}

/// Ordered projection of ranked rows: partition keys, ranked value, then
/// item identity. TopK retains rows; partition keys alone are not unique.
pub fn ranked_row_projection(
    child: &OperatorNode,
    reduction: &Reduction,
    output_name: &str,
) -> Result<Vec<ProjectItem>, crate::ir::SchemaDerivationError> {
    use crate::ir::SchemaDerivationError;
    let value = ranked_column(child)
        .map_err(|error| SchemaDerivationError::InvalidScalarSignature(error.to_string()))?;
    let keys = reduction.group_keys().map_or(&[][..], |keys| keys.keys());
    for &index in keys {
        if index >= child.schema.fields.len() {
            return Err(SchemaDerivationError::InvalidGroupByColumn(
                index,
                child.schema.fields.len(),
            ));
        }
    }
    let mut columns: Vec<ProjectItem> = keys
        .iter()
        .map(|&index| ProjectItem {
            alias: Some(child.schema.fields[index].name.clone()),
            expr: ScalarExpr::Column(index),
        })
        .collect();
    columns.push(ProjectItem {
        alias: Some(output_name.into()),
        expr: ScalarExpr::Column(value),
    });
    let promql = reads_only_time_series(child);
    for (index, field) in child.schema.fields.iter().enumerate() {
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
        &child.schema,
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
) -> crate::ir::schema::SummaryInputExpr {
    use crate::ir::schema::SummaryInputExpr;
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

fn reads_only_time_series(node: &OperatorNode) -> bool {
    match node.non_asap() {
        Some(NonASAPOp::Scan { source, .. }) => matches!(source, Source::TimeSeries { .. }),
        _ => node
            .children()
            .iter()
            .all(|child| reads_only_time_series(child)),
    }
}

/// Exact top-k `node` (an `Aggregate` with one `TopK` measure per listed
/// keys, no filter and no `HAVING`) over `child` (its input as it runs), as
/// it runs: `Limit(k)` over a descending `Sort` of `child` by its ranked
/// column, partitioned by the keys, under a `Project` restoring the logical
/// `[keys.., topk_k]` output (the ranked value under `topk_k`). `Err` says
/// what is unsupported.
pub fn exact_topk_as_sorted_limit(
    node: &OperatorNode,
    child: Rc<OperatorNode>,
) -> Result<Rc<OperatorNode>, String> {
    let Some(NonASAPOp::Aggregate {
        reduction,
        measures,
        filters,
        having,
        ..
    }) = node.non_asap()
    else {
        return Err("not an aggregate".into());
    };
    let Reduction::Reduce(keys) = reduction else {
        return Err("per-entity reduction".into());
    };
    let [AggIntent::TopK { k, .. }] = measures.as_slice() else {
        return Err("measures besides the top-k".into());
    };
    if filters.iter().any(Option::is_some) {
        return Err("filtered measure".into());
    }
    if having.is_some() {
        return Err("HAVING".into());
    }
    let value = ranked_column(&child).map_err(|e| format!("no ranked column: {e}"))?;
    let sorted = Rc::new(
        OperatorNode::new(Operator::NonASAP(NonASAPOp::Sort {
            keys: vec![SortKey {
                expr: ScalarExpr::Column(value),
                ascending: false,
                nulls_first: false,
            }],
            partition_by: keys.clone(),
            child: Rc::clone(&child),
        }))
        .map_err(|e| e.to_string())?
        .with_guarantee(child.guarantee.clone()),
    );
    let limited = Rc::new(
        OperatorNode::new(Operator::NonASAP(NonASAPOp::Limit {
            n: Some(*k),
            offset: 0,
            partition_by: keys.clone(),
            child: sorted,
        }))
        .map_err(|e| e.to_string())?
        .with_guarantee(node.guarantee.clone()),
    );
    let output_name = node.schema.fields[keys.len()].name.clone();
    let cols = ranked_row_projection(&child, &Reduction::Reduce(keys.clone()), &output_name)
        .map_err(|error| format!("ranked rows: {error}"))?;
    Ok(Rc::new(
        OperatorNode::new(Operator::NonASAP(NonASAPOp::Project {
            cols,
            qualifier: None,
            child: limited,
        }))
        .map_err(|e| e.to_string())?
        .with_guarantee(node.guarantee.clone()),
    ))
}
