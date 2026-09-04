//! Shared post-lowering canonicalization of the resolved [`QueryExpr`].
//!
//! Both language front ends funnel through [`resolve_root`](super::resolve::resolve_root),
//! which runs this pass over the resolved tree. Its job is to erase
//! *structural* differences between semantically identical queries so a
//! post-ASAP binding rule matching on the intent algebra sees one canonical
//! spelling regardless of source language (issue #34).
//!
//! ## Heavy-hitter promotion
//!
//! A count- or sum-ranked "order by the aggregate, take the top k" is an
//! additive heavy-hitter represented by [`AggIntent::TopK`]. Front ends may
//! emit it as an ordinary `Limit { Sort { … Aggregate } }`; this pass promotes
//! that shape to the canonical
//!
//! ```text
//! Aggregate { reduction: Reduce(<partition>), measures: [TopK{k}],
//!             child: Aggregate { measures: [Count|Sum], … } }
//! ```
//!
//! — an outer `TopK` whose explicit `ranking` agrees with the inner additive
//! aggregate. Because the match is positional, aliases do not affect it.

use std::rc::Rc;

use super::agg_intent::{
    is_heavy_hitter_ranking, ranking_measure, AggIntent, RankingMeasure, TopKRanking,
};
use super::expr_ir::{CompareOpKind, ScalarValue};
use super::query_expr::{Predicate, QueryExpr, Reduction, SortKey, WindowFuncKind};
use crate::types::AccuracyTarget;

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

/// A `&mut QueryExpr` out of a child `Rc<QueryExpr>` — clone-on-write via
/// [`Rc::make_mut`]: free (no clone) while `r` is uniquely owned, which is
/// the overwhelmingly common case (a tree `canonicalize` was just handed by
/// value); falls back to cloning just *this* node (its own fields — the
/// grandchildren stay shared `Rc`s, not deep-copied) only when some other
/// owner still holds the same `Rc`, e.g. a caller that kept its own clone
/// around (`once.clone()` in `is_idempotent` below — `QueryExpr::clone()` is
/// now a cheap `Rc`-bump, not a deep copy, so that clone shares structure
/// with `once` until a rewrite here needs to touch it). `Rc::get_mut` would
/// panic on exactly that case; `make_mut` degrades to a shallow copy instead
/// of requiring sole ownership as a precondition. Once a workload-level CSE
/// pass runs (issue #212, #222) and canonicalize sees an already-shared
/// subtree from a *different* query, this is also the mechanism that keeps
/// canonicalizing one query from silently corrupting another's view of the
/// same shared node.
fn rc_mut(r: &mut Rc<QueryExpr>) -> &mut QueryExpr {
    Rc::make_mut(r)
}

/// Mutable references to the direct **operator** `QueryExpr` children of a
/// node — `canon`'s own top-down/bottom-up walk only ever visits the
/// relational skeleton, never descending into a scalar position (`Filter.pred`,
/// `ProjectItem.expr`, …): none of the three rewrite rules rewrite anything
/// inside a scalar subtree, so there's nothing to gain by recursing into one,
/// and every scalar variant (issue #205) hits the catch-all below.
fn children_mut(expr: &mut QueryExpr) -> Vec<&mut QueryExpr> {
    use QueryExpr::*;
    match expr {
        // `PromqlScalarBridge`'s child is a scalar-sub-language node (issue
        // #220), not the relational skeleton — same "no children to recurse
        // into" treatment as the scalar variants below.
        Scan { .. } | EvalTimestamp | CurrentTimestamp | PromqlScalarBridge(_) => vec![],
        PromqlVectorFromScalar(c) | PromqlScalarFromVector(c) => vec![rc_mut(c)],
        PromqlRelabel { child, .. }
        | Filter { child, .. }
        | Project { child, .. }
        | Aggregate { child, .. }
        | Dedup { child, .. }
        | PromqlSubquery { child, .. }
        | TimeRange { child, .. }
        | TimeShift { child, .. }
        | SQLWindowFunc { child, .. }
        | PromqlSeriesSample { child, .. }
        | PromqlInfoEnrich { child, .. }
        | Sort { child, .. }
        | Limit { child, .. } => vec![rc_mut(child)],
        Concat { children } => children.iter_mut().collect(),
        Join { left, right, .. } | SetOp { left, right, .. } => {
            vec![rc_mut(left), rc_mut(right)]
        }
        BinaryOp { lhs, rhs, .. } => vec![rc_mut(lhs), rc_mut(rhs)],
        Column(_)
        | Literal(_)
        | Compare { .. }
        | BoolAnd(_)
        | BoolOr(_)
        | Not(_)
        | IsNull(_)
        | IsNotNull(_)
        | Cast { .. }
        | InList { .. }
        | FunctionCall { .. }
        | Arithmetic { .. }
        | Case { .. } => vec![],
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
        expr: QueryExpr::Column(sort_col),
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
            let QueryExpr::Column(underlying) = &cols.get(*sort_col)?.expr else {
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
        reduction,
        measures,
        ..
    } = agg_expr
    else {
        return None;
    };
    let Reduction::Reduce(by) = reduction else {
        return None;
    };
    let [ranked_agg] = measures.as_slice() else {
        return None;
    };
    if ranked_col != by.len() {
        return None;
    }
    // The heavy-hitter decision — descending, over a measure with a realised
    // heavy-hitter sketch — is the shared rule both front ends' promotions
    // consult (issue #38). So an ascending additive-ranked limit
    // (`ORDER BY COUNT(*) ASC LIMIT k` = bottom-k) stays generic, exactly as
    // PromQL `bottomk(k, count_over_time(…))` does.
    if !is_heavy_hitter_ranking(!ascending, ranking_measure(ranked_agg)) {
        return None;
    }
    let ranking = match ranking_measure(ranked_agg) {
        RankingMeasure::Frequency => TopKRanking::Count,
        RankingMeasure::WeightedSum => TopKRanking::Sum,
        RankingMeasure::NonAdditive => return None,
    };
    let accuracy = match ranked_agg {
        AggIntent::Count { accuracy } => accuracy.clone(),
        // SUM carries no local approximation target. Workload-level accuracy
        // allocation can relax this exact default when choosing a summary.
        AggIntent::Sum { .. } => AccuracyTarget::Exact,
        _ => return None,
    };

    // Outer heavy-hitter `TopK`, grouped by the ranking's partition (empty for a
    // global `ORDER BY … LIMIT k`; the `by` labels for a partitioned `topk by`),
    // over the unchanged inner additive aggregate.
    Some(QueryExpr::Aggregate {
        reduction: Reduction::by(partition_by.to_vec()),
        measures: vec![AggIntent::TopK {
            k: *k,
            ranking,
            accuracy,
        }],
        output_names: Vec::new(),
        having: None,
        child: Rc::new(agg_expr.clone()),
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
    let Predicate(pred_expr) = pred;
    let QueryExpr::Compare { left, op, right } = pred_expr.as_ref() else {
        return None;
    };
    // `rn <= k` (top-k). `rn < k` would be off-by-one; require `<=`.
    if *op != CompareOpKind::Le {
        return None;
    }
    let (QueryExpr::Column(rn_col), QueryExpr::Literal(ScalarValue::Int64(k))) =
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
            let QueryExpr::Column(underlying) = &cols.get(*rn_col)?.expr else {
                return None;
            };
            (child.as_ref(), *underlying)
        }
        other => (other, *rn_col),
    };

    // The filtered column must be a `ROW_NUMBER()` window output — the single
    // column the SQLWindowFunc appends after its input, i.e. the last one.
    let QueryExpr::SQLWindowFunc {
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
        child: Rc::new(QueryExpr::Sort {
            keys: order_by.clone(),
            partition_by: partition_by.clone(),
            child: Rc::new(inner.as_ref().clone()),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pre_asap::query_expr::{
        GroupKeys, ProjectItem, Source, WindowFrame, WindowFrameBound, WindowFrameOffset,
        WindowFrameUnits,
    };
    use crate::pre_asap::schema::{Column, DataType, Schema};
    use crate::types::AccuracyTarget;

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
            measures: vec![AggIntent::Count {
                accuracy: AccuracyTarget::Exact,
            }],
            output_names: vec![],
            having: None,
            child: Rc::new(scan()),
        }
    }

    fn desc(col: usize) -> Vec<SortKey> {
        vec![SortKey {
            expr: QueryExpr::Column(col),
            ascending: false,
            nulls_first: false,
        }]
    }

    fn limit(n: usize, offset: usize, child: QueryExpr) -> QueryExpr {
        QueryExpr::Limit {
            n,
            offset,
            child: Rc::new(child),
        }
    }

    fn sort(keys: Vec<SortKey>, child: QueryExpr) -> QueryExpr {
        QueryExpr::Sort {
            keys,
            partition_by: GroupKeys::by(vec![]),
            child: Rc::new(child),
        }
    }

    fn is_topk_over_count(qe: &QueryExpr) -> bool {
        matches!(qe,
            QueryExpr::Aggregate { measures, child, .. }
                if matches!(measures.as_slice(), [AggIntent::TopK { k: 5, .. }])
                && matches!(child.as_ref(), QueryExpr::Aggregate { measures, .. }
                    if matches!(measures.as_slice(), [AggIntent::Count { .. }])))
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
                    expr: QueryExpr::Column(0),
                },
                ProjectItem {
                    alias: Some("c".into()),
                    expr: QueryExpr::Column(1),
                },
            ],
            qualifier: None,
            child: Rc::new(count_by_service()),
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
        // Ascending = bottom-k: the shared `is_heavy_hitter_ranking` rule
        // rejects it (needs descending), so it stays a generic Sort+Limit — the
        // same call PromQL `bottomk` makes (issue #38).
        let asc = vec![SortKey {
            expr: QueryExpr::Column(1),
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
    fn promotes_sum_ranked_limit_sort_with_explicit_ranking_basis() {
        let sum = QueryExpr::Aggregate {
            reduction: Reduction::by(vec![1]),
            measures: vec![AggIntent::Sum { col: None }],
            output_names: vec![],
            having: None,
            child: Rc::new(scan()),
        };
        let q = limit(5, 0, sort(desc(1), sum));
        assert!(matches!(canonicalize(q),
            QueryExpr::Aggregate { measures, child, .. }
                if matches!(measures.as_slice(), [AggIntent::TopK {
                    k: 5,
                    ranking: TopKRanking::Sum,
                    ..
                }])
                && matches!(child.as_ref(), QueryExpr::Aggregate { measures, .. }
                    if matches!(measures.as_slice(), [AggIntent::Sum { .. }]))));
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
            measures: vec![agg],
            output_names: vec![],
            having: None,
            child: Rc::new(scan4()),
        }
    }

    /// `ROW_NUMBER` ignores its frame clause, so the top-k rewrite doesn't care
    /// what's in it; any concrete frame works as fixture data.
    fn rownumber_frame() -> WindowFrame {
        WindowFrame {
            units: WindowFrameUnits::Rows,
            start_bound: WindowFrameBound::Preceding(WindowFrameOffset::Scalar(ScalarValue::Null)),
            end_bound: WindowFrameBound::Following(WindowFrameOffset::Scalar(ScalarValue::Null)),
        }
    }

    /// `Filter{ rn(3) <= 5 } { SQLWindowFunc{ RowNumber, PARTITION BY region(2),
    /// ORDER BY col(2) DESC } { agg } }`.
    fn rownumber_topk(agg: QueryExpr) -> QueryExpr {
        let wf = QueryExpr::SQLWindowFunc {
            func: WindowFuncKind::RowNumber,
            args: vec![],
            partition_by: GroupKeys::by(vec![2]), // region
            order_by: vec![SortKey {
                expr: QueryExpr::Column(2), // the aggregate output column
                ascending: false,
                nulls_first: true,
            }],
            frame: Some(rownumber_frame()),
            output_name: "rn".into(),
            child: Rc::new(agg),
        };
        QueryExpr::Filter {
            pred: Predicate(Rc::new(QueryExpr::Compare {
                left: Rc::new(QueryExpr::Column(3)), // rn = the appended window column
                op: CompareOpKind::Le,
                right: Rc::new(QueryExpr::Literal(ScalarValue::Int64(5))),
            })),
            child: Rc::new(wf),
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
            measures,
            child,
            ..
        } = &out
        else {
            panic!("expected outer Aggregate([TopK]), got {out:?}");
        };
        let Reduction::Reduce(by) = reduction else {
            panic!("expected a Reduce grouping, got {reduction:?}");
        };
        assert!(matches!(
            measures.as_slice(),
            [AggIntent::TopK { k: 5, .. }]
        ));
        assert_eq!(**by, vec![2], "outer TopK partitioned by region");
        assert!(
            matches!(child.as_ref(), QueryExpr::Aggregate { measures, .. }
            if matches!(measures.as_slice(), [AggIntent::Count { .. }]))
        );
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
        assert!(
            matches!(child.as_ref(), QueryExpr::Aggregate { measures, .. }
            if matches!(measures.as_slice(), [AggIntent::Avg { .. }]))
        );
    }

    #[test]
    fn filter_on_a_non_rownumber_column_is_left_alone() {
        // `WHERE service_len <= 5` (col 0, not the rn window column) must not be
        // mistaken for a top-k.
        let wf = QueryExpr::SQLWindowFunc {
            func: WindowFuncKind::RowNumber,
            args: vec![],
            partition_by: GroupKeys::by(vec![2]),
            order_by: vec![SortKey {
                expr: QueryExpr::Column(2),
                ascending: false,
                nulls_first: true,
            }],
            frame: Some(rownumber_frame()),
            output_name: "rn".into(),
            child: Rc::new(grouped(AggIntent::Count {
                accuracy: AccuracyTarget::Exact,
            })),
        };
        let q = QueryExpr::Filter {
            pred: Predicate(Rc::new(QueryExpr::Compare {
                left: Rc::new(QueryExpr::Column(0)), // NOT the rn column (index 3)
                op: CompareOpKind::Le,
                right: Rc::new(QueryExpr::Literal(ScalarValue::Int64(5))),
            })),
            child: Rc::new(wf),
        };
        assert!(
            matches!(canonicalize(q), QueryExpr::Filter { .. }),
            "left as a Filter"
        );
    }
}
