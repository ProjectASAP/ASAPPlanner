//! Series-identity realization for the unified dag.
use crate::ir::schema::*;
/// Resolve a PromQL root to rows carrying [`PROMQL_SERIES_IDENTITY`] before
/// candidate search. `closed` describes physical columns here: the final
/// column contains every dynamic source label. It does not assert that the
/// query's projected labels are the full label set.
///
/// This realization supports explicit `by` grouping, `without` grouping and
/// per-series computation. A `without` grouping keeps the identity with its
/// excluded labels and `__name__` removed, computed by the aggregate:
/// `sum without (instance) (m)` turns `{__name__=m, job=a,
/// instance=1, zone=z}` into `{job=a, zone=z}`, and the aggregate's output
/// schema marks that column as computed, not copied
/// (`aggregate_schema::without_output_schema`). Operators that rewrite or
/// implicitly match dynamic label sets require their own realization; they
/// must not accidentally treat the opaque identity as a user label or
/// silently discard it.
///
/// Realization inserts the identity column into nodes whose parents were
/// bound before it, so every parent's column references are renumbered to
/// match: `topk(1, sum without (instance) (m))` binds its sort key to
/// `Column(0)` of `[sum]`; realization makes the aggregate's output
/// `[$promql_series_identity, sum]` and the key `Column(1)`. A reference to
/// a column realization does not keep fails here rather than read another
/// column.
pub fn with_promql_series_identity(
    root: &std::rc::Rc<crate::ir::OperatorNode>,
) -> Result<std::rc::Rc<crate::ir::OperatorNode>, String> {
    use crate::ir::operator::Source;
    use crate::ir::{NonASAPOp, Operator, OperatorNode};
    use std::{collections::HashMap, rc::Rc};
    fn visit(
        node: &Rc<OperatorNode>,
        cache: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
    ) -> Result<Rc<OperatorNode>, String> {
        if let Some(found) = cache.get(&Rc::as_ptr(node)) {
            return Ok(Rc::clone(found));
        }
        let mut error = None;
        let mut operator = node
            .operator
            .map_children(|child| match visit(child, cache) {
                Ok(child) => child,
                Err(e) => {
                    error = Some(e);
                    Rc::clone(child)
                }
            });
        if let Some(error) = error {
            return Err(error);
        }
        if let (Operator::NonASAP(before), Operator::NonASAP(after)) = (&node.operator, &operator) {
            let positions = positions_after_realization(
                &input_scope(&before.inputs()),
                &input_scope(&after.inputs()),
            );
            if positions
                .iter()
                .enumerate()
                .any(|(column, position)| *position != Some(column))
            {
                let mut dropped = None;
                let renumbered = after.map_input_columns(&mut |column| {
                    positions.get(column).copied().flatten().unwrap_or_else(|| {
                        dropped = Some(column);
                        column
                    })
                });
                if let Some(column) = dropped {
                    return Err(format!(
                        "series identity realization does not keep input column {column} \
                         that {} reads",
                        before.kind_name()
                    ));
                }
                operator = Operator::NonASAP(renumbered);
            }
        }
        match &mut operator {
            Operator::NonASAP(NonASAPOp::Scan {
                source: Source::TimeSeries { .. },
                schema,
                ..
            }) => {
                if schema
                    .fields
                    .iter()
                    .any(|field| field.name == PROMQL_SERIES_IDENTITY)
                {
                    if !schema.has_promql_series_identity() {
                        return Err("invalid physical series identity".into());
                    }
                    cache.insert(Rc::as_ptr(node), Rc::clone(node));
                    return Ok(Rc::clone(node));
                }
                if schema.closed {
                    return Err("dynamic series identity requires an open PromQL source".into());
                }
                schema.fields.push(Field::new(
                    PROMQL_SERIES_IDENTITY,
                    FieldDataType::Plain(DataType::Utf8),
                    false,
                ));
                schema.closed = true;
                // A series has at most one sample per timestamp, so the full
                // identity and the timestamp key each row. Pass 2's
                // identical-expression rule reads this key: only a keyed
                // node is legal to share (hash-consing, `Memo::find_or_add_group`).
                if let Some(time) = schema.time_index {
                    schema.add_unique_key(vec![schema.fields.len() - 1, time]);
                }
            }
            Operator::NonASAP(NonASAPOp::Sort { partition_by, .. })
                if partition_by.is_without() =>
            {
                return Err("dynamic without ranking requires label-set projection".into());
            }
            Operator::NonASAP(
                NonASAPOp::TimeRange { .. }
                | NonASAPOp::Limit { .. }
                | NonASAPOp::Project { .. }
                | NonASAPOp::Filter { .. }
                | NonASAPOp::TimeShift { .. }
                | NonASAPOp::PromqlSubquery { .. }
                | NonASAPOp::PromqlRelabel { .. }
                | NonASAPOp::PromqlVectorFromScalar(_)
                | NonASAPOp::BinaryOp { .. }
                | NonASAPOp::Concat { .. }
                | NonASAPOp::Aggregate { .. }
                | NonASAPOp::Sort { .. },
            ) => {}
            _ => return Err("operator has no dynamic series-identity realization".into()),
        }
        let rebuilt = Rc::new(OperatorNode::new(operator).map_err(|e| e.to_string())?);
        // `having` indexes the aggregate's own output, which the
        // renumbering above does not follow; PromQL emits none.
        if matches!(
            rebuilt.non_asap(),
            Some(NonASAPOp::Aggregate {
                having: Some(_),
                ..
            })
        ) && rebuilt.schema.fields.len() != node.schema.fields.len()
        {
            return Err("series identity realization moves the columns a HAVING reads".into());
        }
        cache.insert(Rc::as_ptr(node), Rc::clone(&rebuilt));
        Ok(rebuilt)
    }
    visit(root, &mut HashMap::new())
}

/// The input scope of an operator over `inputs`: their output columns, in
/// order.
fn input_scope<'a>(inputs: &[&'a std::rc::Rc<crate::ir::OperatorNode>]) -> Vec<&'a Field> {
    inputs
        .iter()
        .flat_map(|input| input.schema.fields.iter())
        .collect()
}

/// Where each column of the input scope `before` realization sits in the
/// scope `after` it: the same columns in the same order, with
/// [`PROMQL_SERIES_IDENTITY`] columns inserted. `[sum]` before and
/// `[$promql_series_identity, sum]` after give `[Some(1)]`. `None` marks a
/// column `after` does not keep.
fn positions_after_realization(before: &[&Field], after: &[&Field]) -> Vec<Option<ColumnId>> {
    let mut next = 0;
    before
        .iter()
        .map(|field| {
            while after
                .get(next)
                .is_some_and(|f| f.name != field.name && f.name == PROMQL_SERIES_IDENTITY)
            {
                next += 1;
            }
            let kept = after.get(next).is_some_and(|f| f.name == field.name);
            kept.then(|| {
                next += 1;
                next - 1
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;
    use std::time::Duration;

    use super::with_promql_series_identity;
    use crate::ir::operator::arguments::{GroupKeys, Reduction, Source};
    use crate::ir::operator::AggIntent;
    use crate::ir::scalar::{Predicate, ScalarValue, SortKey};
    use crate::ir::schema::{DataType, Field, Schema, PROMQL_SERIES_IDENTITY};
    use crate::ir::{NonASAPOp, Operator, OperatorNode, ProjectItem, ScalarExpr, TimeRangeKind};

    fn node(operator: NonASAPOp) -> Rc<OperatorNode> {
        OperatorNode::new_shared(Operator::NonASAP(operator)).unwrap()
    }

    /// `sort_desc(sum without (instance) (m))` with a filter and a
    /// projection above the aggregate, bound over its output `[sum]` before
    /// realization: every reference to `sum`, `Column(0)`, reads
    /// `Column(1)` of the realized `[$promql_series_identity, sum]`.
    #[test]
    fn realization_renumbers_parent_references_into_a_without_aggregate() {
        let scan = node(NonASAPOp::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Field::plain("ts", DataType::Timestamp, false),
                    Field::plain("value", DataType::Float64, false),
                    Field::plain("instance", DataType::Utf8, true),
                ],
                0,
                vec![],
            ),
        });
        let instant = node(NonASAPOp::TimeRange {
            range: Duration::from_secs(300),
            kind: TimeRangeKind::Instant,
            child: scan,
        });
        let sum = node(NonASAPOp::Aggregate {
            reduction: Reduction::Reduce(GroupKeys::without(vec![2])),
            measures: vec![AggIntent::Sum { col: None }],
            output_names: vec![String::new()],
            filters: vec![None],
            having: None,
            child: instant,
        });
        let sum_column = |node: &OperatorNode| {
            node.schema
                .fields
                .iter()
                .position(|field| field.name == "sum")
                .unwrap()
        };
        assert_eq!(sum_column(&sum), 0);
        let filtered = node(NonASAPOp::Filter {
            pred: Predicate(ScalarExpr::IsNotNull(Box::new(ScalarExpr::Column(0)))),
            child: sum,
        });
        let projected = node(NonASAPOp::Project {
            cols: vec![ProjectItem {
                alias: Some("sum".into()),
                expr: ScalarExpr::Column(0),
            }],
            child: Rc::clone(&filtered),
        });
        let sorted = node(NonASAPOp::Sort {
            keys: vec![SortKey {
                expr: ScalarExpr::Column(0),
                ascending: false,
                nulls_first: false,
            }],
            partition_by: GroupKeys::none(),
            child: filtered,
        });
        let sorted = with_promql_series_identity(&sorted).unwrap();
        let Some(NonASAPOp::Sort { keys, child, .. }) = sorted.non_asap() else {
            panic!("a sort");
        };
        assert_eq!(child.schema.fields[0].name, PROMQL_SERIES_IDENTITY);
        assert_eq!(keys[0].expr, ScalarExpr::Column(sum_column(child)));
        let Some(NonASAPOp::Filter { pred, child: sum }) = child.non_asap() else {
            panic!("a filter");
        };
        assert_eq!(
            pred.0,
            ScalarExpr::IsNotNull(Box::new(ScalarExpr::Column(sum_column(sum))))
        );
        let projected = with_promql_series_identity(&projected).unwrap();
        let Some(NonASAPOp::Project { cols, .. }) = projected.non_asap() else {
            panic!("a projection");
        };
        assert_eq!(cols[0].expr, ScalarExpr::Column(1));
    }

    // A direct scalar literal (`vector(1)`) stays a valid vector input when
    // series identity is realized.
    #[test]
    fn series_identity_accepts_direct_vector_literal() {
        let root = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::PromqlVectorFromScalar(
            ScalarExpr::Literal(ScalarValue::Float64(1.0)),
        )))
        .unwrap();
        assert!(with_promql_series_identity(&root).is_ok());
    }
}
