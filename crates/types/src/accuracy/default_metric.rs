//! The metric a query result is most naturally stated in, found by walking
//! the query's operators.

use crate::ir::operator::AggIntent;
use crate::ir::{NonASAPOp, Operator, OperatorNode, QueryRoot, ScalarExpr};

use super::AccuracyMetric;

/// The metric a query's result is most naturally stated in, from the
/// computation at `root`: an L1-scaled absolute error for counts and top-k
/// frequencies, a relative error for distinct counts and L2 frequency
/// moments, and an absolute value error for anything else (sums,
/// arithmetic, rates, averages, extrema, entropy). `None` for a quantile
/// root, whose error is as naturally a rank as a relative one.
///
/// Used only to size and to display: a target whose metric is not set
/// accepts a guarantee in any metric.
pub fn default_metric_for_root(root: &QueryRoot) -> Option<AccuracyMetric> {
    match root {
        QueryRoot::Operator(node) => node_metric(node),
        QueryRoot::Scalar(ScalarExpr::PromqlScalarFromVector(node))
        | QueryRoot::Scalar(ScalarExpr::ScalarSubquery(node)) => node_metric(node),
        QueryRoot::Scalar(_) => Some(AccuracyMetric::ABSOLUTE),
    }
}

/// The default metric of an intent's value, `None` for a quantile and for
/// intents without a listed default.
fn intent_metric(intent: &AggIntent) -> Option<AccuracyMetric> {
    match intent {
        AggIntent::Count | AggIntent::TopK { .. } => Some(AccuracyMetric::ABSOLUTE_STREAM_L1),
        AggIntent::Cardinality { .. } | AggIntent::FrequencyL2 { .. } => {
            Some(AccuracyMetric::Relative)
        }
        _ => None,
    }
}

/// The metric of `node`'s values, looking through operators that only
/// select, order or rename rows.
fn node_metric(node: &OperatorNode) -> Option<AccuracyMetric> {
    let Operator::NonASAP(op) = &node.operator else {
        return Some(AccuracyMetric::ABSOLUTE);
    };
    match op {
        NonASAPOp::Filter { child, .. }
        | NonASAPOp::Sort { child, .. }
        | NonASAPOp::Limit { child, .. }
        | NonASAPOp::Dedup { child, .. } => node_metric(child),
        NonASAPOp::Project { cols, child, .. }
            if cols
                .iter()
                .all(|item| matches!(item.expr, ScalarExpr::Column(_))) =>
        {
            node_metric(child)
        }
        // A `CASE` choosing between columns and literals, e.g. SQL's
        // empty-input NULL guard over a statistic cross-joined with an exact
        // count: the metric of the columns its value branches read.
        NonASAPOp::Project { .. } => (0..node.schema.fields.len())
            .map(|column| column_metric(node, column))
            .collect::<Option<Vec<_>>>()
            .map(|metrics| match metrics.split_first() {
                Some((first, rest)) if rest.iter().all(|metric| metric == first) => *first,
                _ => AccuracyMetric::ABSOLUTE,
            }),
        NonASAPOp::Aggregate { measures, .. } => {
            let mut metrics = Vec::new();
            for intent in measures {
                match intent {
                    AggIntent::Quantile { .. } | AggIntent::HistogramQuantile { .. } => {
                        return None
                    }
                    intent => {
                        metrics.push(intent_metric(intent).unwrap_or(AccuracyMetric::ABSOLUTE))
                    }
                }
            }
            // Measures with different default metrics share one value-unit
            // statement.
            Some(match metrics.split_first() {
                Some((first, rest)) if rest.iter().all(|metric| metric == first) => *first,
                _ => AccuracyMetric::ABSOLUTE,
            })
        }
        _ => Some(AccuracyMetric::ABSOLUTE),
    }
}

/// The metric of `node`'s output `column`, following columns through
/// projections, `CASE` value branches, joins and operators that only
/// select, order or rename rows, down to an aggregate's measure. A group
/// key or any other expression is absolute.
pub(super) fn column_metric(node: &OperatorNode, column: usize) -> Option<AccuracyMetric> {
    use crate::ir::operator::arguments::JoinKind;
    let Operator::NonASAP(op) = &node.operator else {
        return Some(AccuracyMetric::ABSOLUTE);
    };
    match op {
        NonASAPOp::Filter { child, .. }
        | NonASAPOp::Sort { child, .. }
        | NonASAPOp::Limit { child, .. }
        | NonASAPOp::Dedup { child, .. } => column_metric(child, column),
        NonASAPOp::Project { cols, child, .. } => match cols.get(column).map(|item| &item.expr) {
            Some(ScalarExpr::Column(id)) => column_metric(child, *id),
            Some(ScalarExpr::Case {
                branches,
                else_expr,
                ..
            }) => {
                let mut metrics = Vec::new();
                for value in branches
                    .iter()
                    .map(|(_, then)| then)
                    .chain(else_expr.as_deref())
                {
                    match value {
                        // A literal, possibly cast (SQL's typed NULL).
                        ScalarExpr::Literal(_) => {}
                        ScalarExpr::Cast { expr, .. }
                            if matches!(**expr, ScalarExpr::Literal(_)) => {}
                        ScalarExpr::Column(id) => metrics.push(column_metric(child, *id)?),
                        _ => return Some(AccuracyMetric::ABSOLUTE),
                    }
                }
                Some(match metrics.split_first() {
                    Some((first, rest)) if rest.iter().all(|metric| metric == first) => *first,
                    _ => AccuracyMetric::ABSOLUTE,
                })
            }
            _ => Some(AccuracyMetric::ABSOLUTE),
        },
        NonASAPOp::Join {
            kind:
                JoinKind::Inner | JoinKind::Left | JoinKind::Right | JoinKind::Full | JoinKind::Cross,
            left,
            right,
            ..
        } => {
            let width = left.schema.fields.len();
            if column < width {
                column_metric(left, column)
            } else {
                column_metric(right, column - width)
            }
        }
        NonASAPOp::Aggregate {
            reduction,
            measures,
            child,
            ..
        } => match aggregate_measure_at(node, reduction, measures, child, column) {
            Some(AggIntent::Quantile { .. } | AggIntent::HistogramQuantile { .. }) => None,
            Some(intent) => Some(intent_metric(intent).unwrap_or(AccuracyMetric::ABSOLUTE)),
            None => Some(AccuracyMetric::ABSOLUTE),
        },
        _ => Some(AccuracyMetric::ABSOLUTE),
    }
}

/// The measure the aggregate `node`'s output `column` holds, or `None` for
/// a group key, a kept label or `count_values`' synthesized label, as
/// [`aggregate_output_schema`](crate::ir::schema::aggregate_schema::aggregate_output_schema)
/// lays the columns out. A per-entity reduction replaces its one measure's
/// input column in place and keeps every other column; a cross-series one
/// emits its kept labels first, then one column per measure, with a
/// `count_values` label (unless an earlier column has its name) before its
/// count.
fn aggregate_measure_at<'a>(
    node: &OperatorNode,
    reduction: &crate::ir::operator::arguments::Reduction,
    measures: &'a [AggIntent],
    child: &OperatorNode,
    column: usize,
) -> Option<&'a AggIntent> {
    use crate::ir::operator::arguments::Reduction;
    use crate::ir::scalar::{resolve_column_ref, ColumnRef};
    let fields = &node.schema.fields;
    match reduction {
        Reduction::PerEntity => {
            let [measure] = measures else { return None };
            let value = match measure.input_cols().first() {
                Some(input) => *input,
                None => resolve_column_ref(&ColumnRef::SampleValue, &child.schema).ok()?,
            };
            (column == value).then_some(measure)
        }
        Reduction::Reduce(by) => {
            let mut next = if by.is_without() {
                fields.len().checked_sub(measures.len())?
            } else {
                by.len()
            };
            for measure in measures {
                if let AggIntent::CountValues { label } = measure {
                    if !by.is_without()
                        && !fields[..next.min(fields.len())]
                            .iter()
                            .any(|f| f.name == *label)
                    {
                        next += 1;
                    }
                }
                if next == column {
                    return Some(measure);
                }
                next += 1;
            }
            None
        }
    }
}
