//! Series-identity realization for the unified dag.
use crate::ir::schema::error::SchemaDerivationError;
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
/// `[$promql_series_identity, sum]` and the key `Column(1)`. An
/// `Aggregate`'s `having` indexes the aggregate's own output, so it is
/// renumbered by how that output moves (PromQL emits no `having`; an IR
/// tree may).
///
/// Errors:
/// - [`SeriesIdentityError::NotApplicable`]: the root holds an operator
///   this realization does not cover, such as `limitk` or `info()`; a
///   caller plans the root over its label columns instead.
/// - [`SeriesIdentityError::DroppedColumn`]: a reference to a column
///   realization does not keep. Reading another column would give a wrong
///   answer, so a caller must not fall back.
/// - [`SeriesIdentityError::InvalidRealizedOperator`]: a renumbered
///   operator's schema no longer derives. Its input was valid, so this is
///   a defect in realization and a caller must not fall back either.
pub fn with_promql_series_identity(
    root: &std::rc::Rc<crate::ir::OperatorNode>,
) -> Result<std::rc::Rc<crate::ir::OperatorNode>, SeriesIdentityError> {
    use crate::ir::operator::Source;
    use crate::ir::scalar::Predicate;
    use crate::ir::{NonASAPOp, Operator, OperatorNode};
    use std::{collections::HashMap, rc::Rc};
    use SeriesIdentityError::NotApplicable;
    fn visit(
        node: &Rc<OperatorNode>,
        cache: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
    ) -> Result<Rc<OperatorNode>, SeriesIdentityError> {
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
            let positions =
                positions_after_realization(&before.input_scope(), &after.input_scope());
            if moves(&positions) {
                let renumbered = renumber(&positions, before.kind_name(), |column| {
                    after.map_input_columns(&mut |c| column(c))
                })?;
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
                        return Err(NotApplicable("invalid physical series identity".into()));
                    }
                    cache.insert(Rc::as_ptr(node), Rc::clone(node));
                    return Ok(Rc::clone(node));
                }
                if schema.closed {
                    return Err(NotApplicable(
                        "dynamic series identity requires an open PromQL source".into(),
                    ));
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
                return Err(NotApplicable(
                    "dynamic without ranking requires label-set projection".into(),
                ));
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
            Operator::NonASAP(op) => {
                return Err(NotApplicable(format!(
                    "{} has no dynamic series-identity realization",
                    op.kind_name()
                )))
            }
            Operator::ASAP(_) => {
                return Err(NotApplicable(
                    "an ASAP operator has no dynamic series-identity realization".into(),
                ))
            }
        }
        let new = |operator| {
            OperatorNode::new(operator)
                .map(Rc::new)
                .map_err(SeriesIdentityError::InvalidRealizedOperator)
        };
        let mut rebuilt = new(operator)?;
        // `having` indexes the aggregate's own output, which the input
        // renumbering above does not follow.
        if let Some(NonASAPOp::Aggregate {
            having: Some(having),
            ..
        }) = rebuilt.non_asap()
        {
            let positions = positions_after_realization(
                &node.schema.fields.iter().collect::<Vec<_>>(),
                &rebuilt.schema.fields.iter().collect::<Vec<_>>(),
            );
            if moves(&positions) {
                let having = renumber(&positions, "Aggregate's HAVING", |column| {
                    Predicate(having.0.map_columns(&mut |c| column(c)))
                })?;
                let mut aggregate = rebuilt.non_asap().cloned().expect("an aggregate");
                if let NonASAPOp::Aggregate { having: slot, .. } = &mut aggregate {
                    *slot = Some(having);
                }
                rebuilt = new(Operator::NonASAP(aggregate))?;
            }
        }
        cache.insert(Rc::as_ptr(node), Rc::clone(&rebuilt));
        Ok(rebuilt)
    }
    visit(root, &mut HashMap::new())
}

/// Why [`with_promql_series_identity`] gives no root carrying
/// [`PROMQL_SERIES_IDENTITY`].
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SeriesIdentityError {
    /// The root holds an operator this realization does not cover, or a
    /// source that is not an open PromQL source: `limitk(2, m)` lowers to
    /// a `PromqlSeriesSample`, `info(m)` to a `PromqlInfoEnrich`, and a
    /// closed scan has no dynamic labels to carry. The root is planned
    /// over its label columns instead.
    #[error("{0}")]
    NotApplicable(String),
    /// `operator` reads input column `column`, which realization does not
    /// keep. Renumbering it would read another column and give a wrong
    /// answer.
    #[error("series identity realization does not keep column {column} that {operator} reads")]
    DroppedColumn {
        operator: &'static str,
        column: ColumnId,
    },
    /// An operator rebuilt over its realized children, with its column
    /// references renumbered, has a schema that no longer derives. Its
    /// unrealized form derived, and renumbering moves `Column(0)` of
    /// `[sum]` to the same `sum` at `Column(1)` of
    /// `[$promql_series_identity, sum]`, so this is a defect in
    /// realization, not in the query. Planning the unrealized root would
    /// hide it.
    #[error("series identity realization built an invalid operator: {0}")]
    InvalidRealizedOperator(SchemaDerivationError),
}

/// Whether some column of `positions` (see [`positions_after_realization`])
/// moved or is not kept.
fn moves(positions: &[Option<ColumnId>]) -> bool {
    positions
        .iter()
        .enumerate()
        .any(|(column, position)| *position != Some(column))
}

/// `map` run with the renumbering `positions` gives (see
/// [`positions_after_realization`]); [`SeriesIdentityError::DroppedColumn`]
/// when `operator` reads a column `positions` does not keep. Over
/// `[Some(1)]`, `Column(0)` becomes `Column(1)` and `Column(1)` is
/// dropped.
fn renumber<T>(
    positions: &[Option<ColumnId>],
    operator: &'static str,
    map: impl FnOnce(&mut dyn FnMut(ColumnId) -> ColumnId) -> T,
) -> Result<T, SeriesIdentityError> {
    let mut dropped = None;
    let mapped = map(&mut |column| {
        positions.get(column).copied().flatten().unwrap_or_else(|| {
            dropped = Some(column);
            column
        })
    });
    match dropped {
        Some(column) => Err(SeriesIdentityError::DroppedColumn { operator, column }),
        None => Ok(mapped),
    }
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

    use super::{renumber, with_promql_series_identity, SeriesIdentityError};
    use crate::ir::operator::arguments::{GroupKeys, Reduction, Source};
    use crate::ir::operator::AggIntent;
    use crate::ir::scalar::{CompareOpKind, ExprSemantics, Predicate, ScalarValue, SortKey};
    use crate::ir::schema::{DataType, Field, Schema, PROMQL_SERIES_IDENTITY};
    use crate::ir::{NonASAPOp, Operator, OperatorNode, ProjectItem, ScalarExpr, TimeRangeKind};

    fn node(operator: NonASAPOp) -> Rc<OperatorNode> {
        OperatorNode::new_shared(Operator::NonASAP(operator)).unwrap()
    }

    /// `sum without (instance) (m)` over `[ts, value, instance]` with
    /// `having`, which indexes its output: `[sum]` before realization.
    fn sum_without_instance(having: Option<Predicate>) -> Rc<OperatorNode> {
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
        node(NonASAPOp::Aggregate {
            reduction: Reduction::Reduce(GroupKeys::without(vec![2])),
            measures: vec![AggIntent::Sum { col: None }],
            output_names: vec![String::new()],
            filters: vec![None],
            having,
            child: instant,
        })
    }

    /// `sort_desc(sum without (instance) (m))` with a filter and a
    /// projection above the aggregate, bound over its output `[sum]` before
    /// realization: every reference to `sum`, `Column(0)`, reads
    /// `Column(1)` of the realized `[$promql_series_identity, sum]`.
    #[test]
    fn realization_renumbers_parent_references_into_a_without_aggregate() {
        let sum = sum_without_instance(None);
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

    /// `sum without (instance) (m) HAVING sum > 1`: the `having` reads
    /// `sum`, `Column(0)` of the output `[sum]`, and after realization
    /// `Column(1)` of `[$promql_series_identity, sum]`.
    #[test]
    fn realization_renumbers_a_having_over_the_aggregate_output() {
        let sum_over_one = |column| {
            Predicate(ScalarExpr::Compare {
                left: Box::new(ScalarExpr::Column(column)),
                op: CompareOpKind::Gt,
                right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(1.0))),
                semantics: ExprSemantics::Promql,
            })
        };
        let realized =
            with_promql_series_identity(&sum_without_instance(Some(sum_over_one(0)))).unwrap();
        let Some(NonASAPOp::Aggregate {
            having: Some(having),
            ..
        }) = realized.non_asap()
        else {
            panic!("an aggregate with a having");
        };
        assert_eq!(realized.schema.fields[1].name, "sum");
        assert_eq!(*having, sum_over_one(1));
    }

    /// A reference to a column realization does not keep is an error, not a
    /// read of another column: over the positions `[Some(1)]`, `Column(0)`
    /// moves to `Column(1)` and `Column(1)` is dropped.
    #[test]
    fn renumbering_a_dropped_column_is_an_error() {
        let positions = [Some(1)];
        assert_eq!(renumber(&positions, "Sort", |column| column(0)), Ok(1));
        assert_eq!(
            renumber(&positions, "Sort", |column| column(1)),
            Err(SeriesIdentityError::DroppedColumn {
                operator: "Sort",
                column: 1
            })
        );
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
