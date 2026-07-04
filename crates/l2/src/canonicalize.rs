//! Shared post-lowering canonicalization of the L3 [`QueryExpr`].
//!
//! Both language front ends funnel through [`convert_root`](crate::convert_root),
//! which runs this pass over the converted tree. Its job is to erase
//! *structural* differences between semantically identical queries so an L4 rule
//! matching on the intent algebra sees one canonical spelling regardless of
//! source language (issue #34).
//!
//! ## Heavy-hitter promotion
//!
//! A count-ranked "order by the count, take the top k" is the frequency
//! heavy-hitter the [`AggIntent::TopK`] intent represents. Front ends emit it as
//! an ordinary `Limit { Sort { … Aggregate([Count]) } }` (SQL, and PromQL's
//! generic `topk` path); this pass promotes that shape to the canonical
//!
//! ```text
//! Aggregate { by: <partition>, aggs: [TopK{k}],
//!             child: Aggregate { aggs: [Count], … } }
//! ```
//!
//! — an outer `TopK` over the *explicit* inner `Count` (the shape PromQL's
//! `topk(k, count_over_time(…))` already produces). Because the match is
//! **positional** (it checks that the DESC sort key lands on the count's output
//! column) it is oblivious to whether the count was aliased in the source, which
//! is exactly the SQL alias gap (#20) the old front-end gate missed.

use asap_ir::intent_algebra::agg_intent::AggIntent;
use asap_ir::intent_algebra::query_expr::{GroupKeys, QueryExpr, SortKey};
use asap_ir::intent_algebra::L3Expr;

/// Rewrite `expr` into its canonical form (bottom-up). Idempotent: a tree that
/// is already canonical is returned unchanged.
pub fn canonicalize(mut expr: QueryExpr) -> QueryExpr {
    canon(&mut expr);
    expr
}

fn canon(expr: &mut QueryExpr) {
    // Bottom-up: canonicalize every child before matching at this node, so an
    // inner heavy-hitter is promoted before an enclosing rewrite inspects it.
    for child in children_mut(expr) {
        canon(child);
    }
    if let Some(promoted) = try_promote_heavy_hitter(expr) {
        *expr = promoted;
    }
}

/// Mutable references to the direct `QueryExpr` children of a node.
fn children_mut(expr: &mut QueryExpr) -> Vec<&mut QueryExpr> {
    use QueryExpr::*;
    match expr {
        Scan { .. } | Ref { .. } | Scalar(_) | EvalTime => vec![],
        VectorFromScalar(c) | ScalarFromVector(c) => vec![c.as_mut()],
        Relabel { child, .. }
        | Filter { child, .. }
        | Project { child, .. }
        | Aggregate { child, .. }
        | Window { child, .. }
        | Distinct { child, .. }
        | Subquery { child, .. }
        | TimeRange { child, .. }
        | WindowFunc { child, .. }
        | Sort { child, .. }
        | Limit { child, .. } => vec![child.as_mut()],
        LetBinding { expr, child, .. } => vec![expr.as_mut(), child.as_mut()],
        Merge { children } => children.iter_mut().collect(),
        Join { left, right, .. } | SetOp { left, right, .. } => {
            vec![left.as_mut(), right.as_mut()]
        }
        BinaryOp { lhs, rhs, .. } => vec![lhs.as_mut(), rhs.as_mut()],
    }
}

/// Recognise a count-ranked `Limit { Sort { [Project] Aggregate([Count]) } }`
/// and rewrite it to the canonical heavy-hitter `Aggregate([TopK])` over the
/// explicit inner `Count`. Returns `None` when the shape does not match.
fn try_promote_heavy_hitter(expr: &QueryExpr) -> Option<QueryExpr> {
    // Limit k, no offset (an OFFSET means "not the top k").
    let QueryExpr::Limit {
        n: k,
        offset: 0,
        child,
    } = expr
    else {
        return None;
    };
    // A single DESC ordering key.
    let QueryExpr::Sort {
        keys,
        partition_by,
        child: sort_child,
    } = child.as_ref()
    else {
        return None;
    };
    let [SortKey {
        expr: L3Expr::Column(sort_col),
        ascending: false,
        ..
    }] = keys.as_slice()
    else {
        return None;
    };

    // The ordered relation is an `Aggregate`, optionally behind a passthrough
    // projection (a bare-column SELECT list). Map the sort key through the
    // projection to the aggregate's own output column.
    let (agg_expr, ranked_col) = match sort_child.as_ref() {
        QueryExpr::Project { cols, child, .. } => {
            let L3Expr::Column(underlying) = &cols.get(*sort_col)?.expr else {
                return None;
            };
            (child.as_ref(), *underlying)
        }
        other => (other, *sort_col),
    };

    // Exactly one `Count` aggregate, and the DESC key must rank by *its* output
    // column — the count sits at index `by.len()` (after the group keys).
    let QueryExpr::Aggregate { by, aggs, .. } = agg_expr else {
        return None;
    };
    let [AggIntent::Count { accuracy }] = aggs.as_slice() else {
        return None;
    };
    if ranked_col != by.len() {
        return None;
    }

    // Outer heavy-hitter `TopK`, grouped by the ranking's partition (empty for a
    // global `ORDER BY … LIMIT k`; the `by` labels for a partitioned `topk by`),
    // over the *unchanged* inner `Count` aggregate.
    Some(QueryExpr::Aggregate {
        by: GroupKeys(partition_by.to_vec()),
        aggs: vec![AggIntent::TopK {
            k: *k,
            accuracy: accuracy.clone(),
        }],
        output_names: Vec::new(),
        having: None,
        child: Box::new(agg_expr.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_ir::intent_algebra::query_expr::{ProjectItem, Source};
    use asap_ir::intent_algebra::schema::{Column, DataType, Schema};
    use asap_ir::types::AccuracyTarget;

    fn scan() -> QueryExpr {
        QueryExpr::Scan {
            source: Source::TimeSeries {
                metric: "m".into(),
            },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Column::new("ts", DataType::Timestamp, false),
                    Column::new("service", DataType::Utf8, false),
                    Column::new("value", DataType::Float64, false),
                ],
                0,
                vec![],
            ),
        }
    }

    /// `Aggregate{ by: [1], [Count] }` over the scan — output cols `[service, count]`.
    fn count_by_service() -> QueryExpr {
        QueryExpr::Aggregate {
            by: GroupKeys(vec![1]),
            aggs: vec![AggIntent::Count {
                accuracy: AccuracyTarget::Exact,
            }],
            output_names: vec![],
            having: None,
            child: Box::new(scan()),
        }
    }

    fn desc(col: usize) -> Vec<SortKey> {
        vec![SortKey {
            expr: L3Expr::Column(col),
            ascending: false,
            nulls_first: false,
        }]
    }

    fn limit(n: usize, offset: usize, child: QueryExpr) -> QueryExpr {
        QueryExpr::Limit {
            n,
            offset,
            child: Box::new(child),
        }
    }

    fn sort(keys: Vec<SortKey>, child: QueryExpr) -> QueryExpr {
        QueryExpr::Sort {
            keys,
            partition_by: GroupKeys(vec![]),
            child: Box::new(child),
        }
    }

    fn is_topk_over_count(qe: &QueryExpr) -> bool {
        matches!(qe,
            QueryExpr::Aggregate { aggs, child, .. }
                if matches!(aggs.as_slice(), [AggIntent::TopK { k: 5, .. }])
                && matches!(child.as_ref(), QueryExpr::Aggregate { aggs, .. }
                    if matches!(aggs.as_slice(), [AggIntent::Count { .. }])))
    }

    #[test]
    fn promotes_count_ranked_limit_sort() {
        // Limit 5 { Sort DESC by count-col (1) { Aggregate[Count] by [1] } }.
        let q = limit(5, 0, sort(desc(1), count_by_service()));
        assert!(is_topk_over_count(&canonicalize(q)));
    }

    #[test]
    fn promotes_through_a_passthrough_projection() {
        // …with a `SELECT service, count` projection between the Sort and the Agg.
        let proj = QueryExpr::Project {
            cols: vec![
                ProjectItem { alias: None, expr: L3Expr::Column(0) },
                ProjectItem { alias: Some("c".into()), expr: L3Expr::Column(1) },
            ],
            qualifier: None,
            child: Box::new(count_by_service()),
        };
        let q = limit(5, 0, sort(desc(1), proj));
        assert!(is_topk_over_count(&canonicalize(q)));
    }

    #[test]
    fn is_idempotent() {
        let q = limit(5, 0, sort(desc(1), count_by_service()));
        let once = canonicalize(q);
        let twice = canonicalize(once.clone());
        assert_eq!(once, twice, "canonicalize must be idempotent");
    }

    #[test]
    fn does_not_promote_ascending_sort() {
        let asc = vec![SortKey { expr: L3Expr::Column(1), ascending: true, nulls_first: false }];
        let q = limit(5, 0, sort(asc, count_by_service()));
        assert!(!is_topk_over_count(&canonicalize(q)));
    }

    #[test]
    fn does_not_promote_with_offset() {
        let q = limit(5, 2, sort(desc(1), count_by_service()));
        assert!(!is_topk_over_count(&canonicalize(q)));
    }

    #[test]
    fn does_not_promote_ranking_by_a_group_key() {
        // DESC by col 0 (the `service` group key), not the count → not a
        // frequency heavy-hitter.
        let q = limit(5, 0, sort(desc(0), count_by_service()));
        assert!(!is_topk_over_count(&canonicalize(q)));
    }

    #[test]
    fn does_not_promote_non_count_aggregate() {
        let sum = QueryExpr::Aggregate {
            by: GroupKeys(vec![1]),
            aggs: vec![AggIntent::Sum { col: None }],
            output_names: vec![],
            having: None,
            child: Box::new(scan()),
        };
        let q = limit(5, 0, sort(desc(1), sum));
        assert!(!is_topk_over_count(&canonicalize(q)));
    }
}
