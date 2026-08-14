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
//! Aggregate { reduction: Reduce(<partition>), aggs: [TopK{k}],
//!             child: Aggregate { aggs: [Count], … } }
//! ```
//!
//! — an outer `TopK` over the *explicit* inner `Count` (the shape PromQL's
//! `topk(k, count_over_time(…))` already produces). Because the match is
//! **positional** (it checks that the DESC sort key lands on the count's output
//! column) it is oblivious to whether the count was aliased in the source, which
//! is exactly the SQL alias gap (#20) the old front-end gate missed.

use asap_types::intent_algebra::agg_intent::{
    is_frequency_heavy_hitter, ranking_measure, AggIntent,
};
use asap_types::intent_algebra::expr_ir::{CompareOp, L3Scalar};
use asap_types::intent_algebra::query_expr::{
    Predicate, QueryExpr, Reduction, SortKey, WindowFuncKind,
};
use asap_types::intent_algebra::L3Expr;

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
    // Local rewrites chain: a `ROW_NUMBER()`-partitioned top-k rewrites to a
    // `Limit{Sort}`, which the heavy-hitter rule may then promote to an
    // `Aggregate([TopK])`. Each rule strictly simplifies the node, so applying
    // them to a fixpoint terminates.
    while let Some(rewritten) =
        try_rewrite_rownumber_topk(expr).or_else(|| try_promote_heavy_hitter(expr))
    {
        *expr = rewritten;
    }
}

/// Mutable references to the direct `QueryExpr` children of a node.
fn children_mut(expr: &mut QueryExpr) -> Vec<&mut QueryExpr> {
    use QueryExpr::*;
    match expr {
        Scan { .. } | Scalar(_) | EvalTime => vec![],
        VectorFromScalar(c) | ScalarFromVector(c) => vec![c.as_mut()],
        Relabel { child, .. }
        | Filter { child, .. }
        | Project { child, .. }
        | Aggregate { child, .. }
        | Distinct { child, .. }
        | Subquery { child, .. }
        | TimeRange { child, .. }
        | TimeShift { child, .. }
        | WindowFunc { child, .. }
        | Sample { child, .. }
        | InfoJoin { child, .. }
        | Sort { child, .. }
        | Limit { child, .. } => vec![child.as_mut()],
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
    // A single ordering key on a column.
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
        ascending,
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

    // Exactly one aggregate, ranked by *its* output column — the measure sits at
    // index `by.len()` (after the group keys). A `PerEntity` reduction has no
    // `by` to rank a measure against — this shape can't be heavy-hitter
    // promoted, so it's a non-match rather than an error.
    let QueryExpr::Aggregate {
        reduction, aggs, ..
    } = agg_expr
    else {
        return None;
    };
    let Reduction::Reduce(by) = reduction else {
        return None;
    };
    let [ranked_agg] = aggs.as_slice() else {
        return None;
    };
    if ranked_col != by.len() {
        return None;
    }
    // The heavy-hitter decision — descending, over a measure with a realised
    // heavy-hitter sketch — is the shared rule both front ends' promotions
    // consult (issue #38). So an ascending count-ranked limit
    // (`ORDER BY COUNT(*) ASC LIMIT k` = bottom-k) stays generic, exactly as
    // PromQL `bottomk(k, count_over_time(…))` does; and a `sum`-ranked limit
    // stays generic too (`WeightedSum` is additive but not yet sketch-realised),
    // matching today's behaviour until a weighted heavy-hitter lands.
    if !is_frequency_heavy_hitter(!ascending, ranking_measure(ranked_agg)) {
        return None;
    }
    // The only realised heavy-hitter measure is unweighted `Frequency`, so the
    // ranked aggregate is a `Count`; carry its accuracy onto the `TopK`. (When a
    // weighted-sum heavy-hitter lands, this widens alongside `RankingMeasure`.)
    let AggIntent::Count { accuracy } = ranked_agg else {
        return None;
    };

    // Outer heavy-hitter `TopK`, grouped by the ranking's partition (empty for a
    // global `ORDER BY … LIMIT k`; the `by` labels for a partitioned `topk by`),
    // over the *unchanged* inner `Count` aggregate.
    Some(QueryExpr::Aggregate {
        reduction: Reduction::by(partition_by.to_vec()),
        aggs: vec![AggIntent::TopK {
            k: *k,
            accuracy: accuracy.clone(),
        }],
        output_names: Vec::new(),
        having: None,
        child: Box::new(agg_expr.clone()),
    })
}

/// Recognise the SQL partitioned-top-k idiom — `WHERE rn <= k` over a
/// `ROW_NUMBER() OVER (PARTITION BY p ORDER BY o)` — and rewrite it to the
/// generic partitioned top-k `Limit{k} { Sort{ o, partition_by: p } }` (issue
/// #24). The count-ranked case is then promoted to a heavy-hitter `TopK` by
/// [`try_promote_heavy_hitter`], so a SQL `ROW_NUMBER` top-k and the PromQL
/// `topk by (…)` it mirrors converge on the same canonical shape.
fn try_rewrite_rownumber_topk(expr: &QueryExpr) -> Option<QueryExpr> {
    // Filter { pred: `Column(rn) <= k` }.
    let QueryExpr::Filter { pred, child } = expr else {
        return None;
    };
    let Predicate(L3Expr::Compare { left, op, right }) = pred else {
        return None;
    };
    // `rn <= k` (top-k). `rn < k` would be off-by-one; require `<=`.
    if *op != CompareOp::Le {
        return None;
    }
    let (L3Expr::Column(rn_col), L3Expr::Literal(L3Scalar::Int64(k))) =
        (left.as_ref(), right.as_ref())
    else {
        return None;
    };
    if *k < 0 {
        return None;
    }

    // Optionally strip a passthrough projection (the derived table's SELECT that
    // re-exposes the aggregate columns + rn), mapping the rn column through it.
    let (wf_expr, rn_in_wf) = match child.as_ref() {
        QueryExpr::Project { cols, child, .. } => {
            let L3Expr::Column(underlying) = &cols.get(*rn_col)?.expr else {
                return None;
            };
            (child.as_ref(), *underlying)
        }
        other => (other, *rn_col),
    };

    // The filtered column must be a `ROW_NUMBER()` window output — the single
    // column the WindowFunc appends after its input, i.e. the last one.
    let QueryExpr::WindowFunc {
        func: WindowFuncKind::RowNumber,
        partition_by,
        order_by,
        child: inner,
        ..
    } = wf_expr
    else {
        return None;
    };
    if order_by.is_empty() {
        return None;
    }
    let inner_cols = inner.output_schema().ok()?.columns.len();
    if rn_in_wf != inner_cols {
        return None; // the predicate ranks some other column, not the row number
    }

    // Generic partitioned top-k. The window's ORDER BY keys are relative to its
    // input (`inner`), so they transfer directly to a `Sort` over `inner`.
    Some(QueryExpr::Limit {
        n: *k as usize,
        offset: 0,
        child: Box::new(QueryExpr::Sort {
            keys: order_by.clone(),
            partition_by: partition_by.clone(),
            child: Box::new(inner.as_ref().clone()),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::intent_algebra::query_expr::{GroupKeys, ProjectItem, Source};
    use asap_types::intent_algebra::schema::{Column, DataType, Schema};
    use asap_types::types::AccuracyTarget;

    fn scan() -> QueryExpr {
        QueryExpr::Scan {
            source: Source::TimeSeries { metric: "m".into() },
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
            reduction: Reduction::by(vec![1]),
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
            partition_by: GroupKeys::by(vec![]),
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
                ProjectItem {
                    alias: None,
                    expr: L3Expr::Column(0),
                },
                ProjectItem {
                    alias: Some("c".into()),
                    expr: L3Expr::Column(1),
                },
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
        // Ascending = bottom-k: the shared `is_frequency_heavy_hitter` rule
        // rejects it (needs descending), so it stays a generic Sort+Limit — the
        // same call PromQL `bottomk` makes (issue #38).
        let asc = vec![SortKey {
            expr: L3Expr::Column(1),
            ascending: true,
            nulls_first: false,
        }];
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
        // A `sum`-ranked descending limit classifies as `RankingMeasure::
        // WeightedSum` — additive, so a weighted heavy-hitter sketch is possible
        // in principle, but none is realised yet, so it stays a generic
        // Sort+Limit (issue #38). This pins the reserved-but-not-promoted
        // contract: flipping it on is a future weighted-sketch change.
        let sum = QueryExpr::Aggregate {
            reduction: Reduction::by(vec![1]),
            aggs: vec![AggIntent::Sum { col: None }],
            output_names: vec![],
            having: None,
            child: Box::new(scan()),
        };
        let q = limit(5, 0, sort(desc(1), sum));
        assert!(!is_topk_over_count(&canonicalize(q)));
    }

    // ── ROW_NUMBER() partitioned top-k (issue #24) ──────────────────────────

    /// A scan with `[ts, service, region, value]`.
    fn scan4() -> QueryExpr {
        QueryExpr::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Column::new("ts", DataType::Timestamp, false),
                    Column::new("service", DataType::Utf8, false),
                    Column::new("region", DataType::Utf8, false),
                    Column::new("value", DataType::Float64, false),
                ],
                0,
                vec![],
            ),
        }
    }

    /// `Aggregate{ by: [1,2] (service, region), [agg] }` — output `[service,
    /// region, <agg>]` (3 cols), so a ROW_NUMBER over it appends `rn` at index 3.
    fn grouped(agg: AggIntent) -> QueryExpr {
        QueryExpr::Aggregate {
            reduction: Reduction::by(vec![1, 2]),
            aggs: vec![agg],
            output_names: vec![],
            having: None,
            child: Box::new(scan4()),
        }
    }

    /// `Filter{ rn(3) <= 5 } { WindowFunc{ RowNumber, PARTITION BY region(2),
    /// ORDER BY col(2) DESC } { agg } }`.
    fn rownumber_topk(agg: QueryExpr) -> QueryExpr {
        let wf = QueryExpr::WindowFunc {
            func: WindowFuncKind::RowNumber,
            args: vec![],
            partition_by: GroupKeys::by(vec![2]), // region
            order_by: vec![SortKey {
                expr: L3Expr::Column(2), // the aggregate output column
                ascending: false,
                nulls_first: true,
            }],
            output_name: "rn".into(),
            child: Box::new(agg),
        };
        QueryExpr::Filter {
            pred: Predicate(L3Expr::Compare {
                left: Box::new(L3Expr::Column(3)), // rn = the appended window column
                op: CompareOp::Le,
                right: Box::new(L3Expr::Literal(L3Scalar::Int64(5))),
            }),
            child: Box::new(wf),
        }
    }

    #[test]
    fn rownumber_count_topk_becomes_a_partitioned_heavy_hitter() {
        // Count-ranked ROW_NUMBER top-k → outer TopK grouped by the partition
        // (region, col 2) over the explicit inner Count.
        let q = rownumber_topk(grouped(AggIntent::Count {
            accuracy: AccuracyTarget::Exact,
        }));
        let out = canonicalize(q);
        let QueryExpr::Aggregate {
            reduction,
            aggs,
            child,
            ..
        } = &out
        else {
            panic!("expected outer Aggregate([TopK]), got {out:?}");
        };
        let Reduction::Reduce(by) = reduction else {
            panic!("expected a Reduce grouping, got {reduction:?}");
        };
        assert!(matches!(aggs.as_slice(), [AggIntent::TopK { k: 5, .. }]));
        assert_eq!(**by, vec![2], "outer TopK partitioned by region");
        assert!(matches!(child.as_ref(), QueryExpr::Aggregate { aggs, .. }
            if matches!(aggs.as_slice(), [AggIntent::Count { .. }])));
    }

    #[test]
    fn rownumber_avg_topk_becomes_a_partitioned_sort_limit() {
        // Avg-ranked (not a frequency heavy-hitter) → generic partitioned
        // top-k: Limit{5}{ Sort{ partition_by: [region] } }.
        let q = rownumber_topk(grouped(AggIntent::Avg { col: None }));
        let out = canonicalize(q);
        let QueryExpr::Limit { n, child, .. } = &out else {
            panic!("expected a Limit, got {out:?}");
        };
        assert_eq!(*n, 5);
        let QueryExpr::Sort {
            partition_by,
            child,
            ..
        } = child.as_ref()
        else {
            panic!("expected a Sort under the Limit");
        };
        assert_eq!(**partition_by, vec![2], "partitioned by region");
        assert!(matches!(child.as_ref(), QueryExpr::Aggregate { aggs, .. }
            if matches!(aggs.as_slice(), [AggIntent::Avg { .. }])));
    }

    #[test]
    fn filter_on_a_non_rownumber_column_is_left_alone() {
        // `WHERE service_len <= 5` (col 0, not the rn window column) must not be
        // mistaken for a top-k.
        let wf = QueryExpr::WindowFunc {
            func: WindowFuncKind::RowNumber,
            args: vec![],
            partition_by: GroupKeys::by(vec![2]),
            order_by: vec![SortKey {
                expr: L3Expr::Column(2),
                ascending: false,
                nulls_first: true,
            }],
            output_name: "rn".into(),
            child: Box::new(grouped(AggIntent::Count {
                accuracy: AccuracyTarget::Exact,
            })),
        };
        let q = QueryExpr::Filter {
            pred: Predicate(L3Expr::Compare {
                left: Box::new(L3Expr::Column(0)), // NOT the rn column (index 3)
                op: CompareOp::Le,
                right: Box::new(L3Expr::Literal(L3Scalar::Int64(5))),
            }),
            child: Box::new(wf),
        };
        assert!(
            matches!(canonicalize(q), QueryExpr::Filter { .. }),
            "left as a Filter"
        );
    }
}
