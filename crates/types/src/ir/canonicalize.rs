//! Post-lowering canonicalization of the operator DAG.
//!
//! Erases *structural* differences between semantically identical queries so
//! a post-ASAP binding rule matching on the intent algebra sees one canonical
//! spelling regardless of source language (issue #34).
//!
//! ## Heavy-hitter promotion
//!
//! An additive-ranked "order by the aggregate, take the top k" is a
//! heavy-hitter represented by [`AggIntent::TopK`]. Front ends may emit it as
//! an ordinary `Limit { Sort { … Aggregate } }`; this pass promotes that shape
//! to the canonical
//!
//! ```text
//! Aggregate { reduction: Reduce(<partition>), measures: [TopK{k}],
//!             child: Aggregate { measures: [Count | Sum], … } }
//! ```
//!
//! Count supplies unit weights and Sum supplies value weights. Because the
//! match is positional, aliases do not affect it. Other ranked expressions
//! retain Sort + Limit.

use std::collections::HashMap;
use std::rc::Rc;

use super::node::{Operator, OperatorNode};
use super::non_asap::NonASAPOp;
use super::scalar::{Predicate, ProjectItem, ScalarExpr, SortKey};
use crate::pre_asap::agg_intent::{topk, AggIntent};
use crate::pre_asap::expr_ir::{CompareOpKind, ScalarValue};
use crate::pre_asap::query_expr::{QueryExprError, Reduction, WindowFuncKind};
use crate::types::AccuracyTarget;

/// Rewrite the DAG under `root` into its canonical form (bottom-up).
/// Idempotent: an already-canonical DAG comes back as the same `Rc`. Only
/// nodes that change (or whose inputs change) are rebuilt; every untouched
/// subtree keeps its pointer identity, and a shared subtree that is rewritten
/// stays shared.
pub fn canonicalize(root: Rc<OperatorNode>) -> Result<Rc<OperatorNode>, QueryExprError> {
    canon(&root, &mut HashMap::new())
}

fn canon(
    node: &Rc<OperatorNode>,
    memo: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
) -> Result<Rc<OperatorNode>, QueryExprError> {
    if let Some(done) = memo.get(&Rc::as_ptr(node)) {
        return Ok(Rc::clone(done));
    }

    // A `Concat` asserting a caller-proven `discriminator_unique_key` (issue
    // #228) had that key's `ColumnId`s resolved against exactly the first
    // branch's output schema *as it stood before this pass ran*. The rewrites
    // below can restructure that branch (anywhere within it) into a shape
    // with a different output schema, which would leave those `ColumnId`s
    // pointing at the wrong column, or out of bounds. Snapshot the schema the
    // key was resolved against before recursing into the children.
    let discriminator_branch_schema_before = match &node.operator {
        Operator::NonASAP(NonASAPOp::Concat {
            children,
            discriminator_unique_key: Some(_),
        }) => children.first().map(|c| c.schema.clone()),
        _ => None,
    };

    // Bottom-up: canonicalize every operator input before matching at this
    // node, so an inner heavy-hitter is promoted before an enclosing rewrite
    // inspects it.
    let mut rebuilt: Vec<(*const OperatorNode, Rc<OperatorNode>)> = Vec::new();
    let mut changed = false;
    for child in operator_children(&node.operator) {
        let new = canon(child, memo)?;
        changed |= !Rc::ptr_eq(&new, child);
        rebuilt.push((Rc::as_ptr(child), new));
    }
    let mut current = if changed {
        // `map_children` also visits the operator nodes referenced from
        // scalar expressions; those are not in `rebuilt` and pass through
        // unchanged. (A node that is both an operator input and a scalar
        // reference is one shared node, so it takes its canonical form in
        // both places.)
        let rebuilt_child = |c: &Rc<OperatorNode>| {
            rebuilt
                .iter()
                .find(|(ptr, _)| *ptr == Rc::as_ptr(c))
                .map_or_else(|| Rc::clone(c), |(_, new)| Rc::clone(new))
        };
        Rc::new(node.map_children(rebuilt_child)?)
    } else {
        Rc::clone(node)
    };

    // If the first branch's output schema moved out from under the asserted
    // key, the key can no longer be trusted — drop it (never re-derive it by
    // guessing at name/position). A wrong `unique_keys` claim is a wrong
    // query answer, not a missed optimization, so any difference at all
    // drops the key.
    if let Operator::NonASAP(NonASAPOp::Concat {
        children,
        discriminator_unique_key: Some(_),
    }) = &current.operator
    {
        let after = children.first().map(|c| &c.schema);
        if discriminator_branch_schema_before.as_ref() != after {
            current = OperatorNode::non_asap_node(NonASAPOp::Concat {
                children: children.clone(),
                discriminator_unique_key: None,
            })?;
        }
    }

    // Local rewrites chain: a `ROW_NUMBER()`-partitioned top-k rewrites to a
    // `Limit{Sort}`, which the heavy-hitter rule may then promote to an
    // `Aggregate([TopK])`. Each rule strictly simplifies the node, so
    // applying them to a fixpoint terminates.
    loop {
        let next = match try_rewrite_rownumber_topk(&current)? {
            Some(next) => next,
            None => match try_promote_additive_top_ranking(&current)? {
                Some(next) => next,
                None => break,
            },
        };
        current = next;
    }

    memo.insert(Rc::as_ptr(node), Rc::clone(&current));
    Ok(current)
}

/// The direct **operator** inputs of a node — the relational skeleton only.
/// Operator nodes referenced from a scalar position (`ScalarSubquery`,
/// `Exists`, …) are not visited: none of the rewrite rules here rewrites
/// anything inside a scalar subtree.
fn operator_children(op: &Operator) -> Vec<&Rc<OperatorNode>> {
    use NonASAPOp::*;
    match op {
        Operator::ASAP(op) => op.children(),
        Operator::NonASAP(op) => match op {
            Scan { .. } | Values { .. } | PromqlVectorFromScalar(_) | ScalarBridge(_) => vec![],
            Filter { child, .. }
            | Project { child, .. }
            | Aggregate { child, .. }
            | Dedup { child, .. }
            | Sort { child, .. }
            | Limit { child, .. }
            | SQLWindowFunc { child, .. }
            | TimeRange { child, .. }
            | TimeShift { child, .. }
            | PromqlRelabel { child, .. }
            | PromqlInfoEnrich { child, .. }
            | PromqlSeriesSample { child, .. }
            | PromqlSubquery { child, .. } => vec![child],
            Join { left, right, .. } | SetOp { left, right, .. } => vec![left, right],
            BinaryOp { lhs, rhs, .. } => vec![lhs, rhs],
            Concat { children, .. } => children.iter().collect(),
        },
    }
}

/// Recognise an additive-ranked
/// `Limit { Sort { [Project] Aggregate([Count | Sum]) } }` and rewrite it to
/// the canonical heavy-hitter `Aggregate([TopK])` over the explicit inner
/// aggregate. Returns `None` when the shape does not match.
fn try_promote_additive_top_ranking(
    node: &OperatorNode,
) -> Result<Option<Rc<OperatorNode>>, QueryExprError> {
    // Limit k, no offset (an OFFSET means "not the top k").
    let Some(NonASAPOp::Limit {
        n: Some(k),
        offset: 0,
        partition_by: limit_partition,
        child,
    }) = node.non_asap()
    else {
        return Ok(None);
    };
    // A single ordering key on a column.
    let Some(NonASAPOp::Sort {
        keys,
        partition_by,
        child: sort_child,
    }) = child.non_asap()
    else {
        return Ok(None);
    };
    // A per-group `Limit` must agree with its `Sort`'s partition: the
    // ranking's partition is what the outer `TopK` groups by.
    if !limit_partition.is_empty() && limit_partition != partition_by {
        return Ok(None);
    }
    let [SortKey {
        expr: ScalarExpr::Column(sort_col),
        ascending,
        ..
    }] = keys.as_slice()
    else {
        return Ok(None);
    };

    // The ordered relation is an `Aggregate`, optionally behind a passthrough
    // projection (a bare-column SELECT list). Map the sort key through the
    // projection to the aggregate's own output column.
    let (agg_node, ranked_col) = match sort_child.non_asap() {
        Some(NonASAPOp::Project { cols, child, .. }) => {
            let Some(ProjectItem {
                expr: ScalarExpr::Column(underlying),
                ..
            }) = cols.get(*sort_col)
            else {
                return Ok(None);
            };
            (child, *underlying)
        }
        _ => (sort_child, *sort_col),
    };

    // Exactly one aggregate, ranked by *its* output column — the measure sits
    // at index `by.len()` (after the group keys). A `PerEntity` reduction has
    // no `by` to rank a measure against, so it is a non-match.
    let Some(NonASAPOp::Aggregate {
        reduction,
        measures,
        child: aggregate_child,
        ..
    }) = agg_node.non_asap()
    else {
        return Ok(None);
    };
    let Reduction::Reduce(by) = reduction else {
        return Ok(None);
    };
    let [ranked_agg] = measures.as_slice() else {
        return Ok(None);
    };
    if ranked_col != by.len() {
        return Ok(None);
    }
    // The heavy-hitter decision — descending, over a measure with a realised
    // heavy-hitter sketch — is the shared rule both front ends consult (issue
    // #38). An ascending additive-ranked limit (bottom-k) stays generic.
    if !topk::Ranking::from_aggregate(ranked_agg).is_supported(!ascending) {
        return Ok(None);
    }
    // A direct Sum is a stream of additive observation weights. A Sum over a
    // derived child such as Rate/Increase still needs exact reset-aware
    // values to rerank sketch candidates, and the post-ASAP IR has no
    // candidate-sidecar + exact-rerank node, so that shape keeps Sort + Limit.
    if matches!(ranked_agg, AggIntent::Sum { .. })
        && matches!(aggregate_child.non_asap(), Some(NonASAPOp::Aggregate { .. }))
    {
        return Ok(None);
    }
    // Count ranks unit updates; a direct Sum ranks weighted updates.
    let accuracy = match ranked_agg {
        AggIntent::Count { accuracy } => accuracy.clone(),
        AggIntent::Sum { .. } => AccuracyTarget::Exact,
        _ => unreachable!("additive ranking gate admitted a non-additive measure"),
    };

    // Outer heavy-hitter `TopK`, grouped by the ranking's partition (empty for
    // a global `ORDER BY … LIMIT k`; the `by` labels for a partitioned `topk
    // by`), over the unchanged inner additive aggregate.
    OperatorNode::non_asap_node(NonASAPOp::Aggregate {
        reduction: Reduction::by(partition_by.to_vec()),
        measures: vec![AggIntent::TopK { k: *k, accuracy }],
        output_names: Vec::new(),
        having: None,
        child: Rc::clone(agg_node),
    })
    .map(Some)
}

/// Recognise the SQL partitioned-top-k idiom — `WHERE rn <= k` over a
/// `ROW_NUMBER() OVER (PARTITION BY p ORDER BY o)` — and rewrite it to the
/// generic partitioned top-k `Limit{k, partition_by: p} { Sort{ o,
/// partition_by: p } }` (issue #24). The count-ranked case is then promoted
/// to a heavy-hitter `TopK` by [`try_promote_additive_top_ranking`], so a SQL
/// `ROW_NUMBER` top-k and the PromQL `topk by (…)` it mirrors converge on the
/// same canonical shape.
fn try_rewrite_rownumber_topk(
    node: &OperatorNode,
) -> Result<Option<Rc<OperatorNode>>, QueryExprError> {
    // Filter { pred: `Column(rn) <= k` }.
    let Some(NonASAPOp::Filter {
        pred: Predicate(pred_expr),
        child,
    }) = node.non_asap()
    else {
        return Ok(None);
    };
    let ScalarExpr::Compare {
        left, op, right, ..
    } = pred_expr
    else {
        return Ok(None);
    };
    // `rn <= k` (top-k). `rn < k` would be off-by-one; require `<=`.
    if *op != CompareOpKind::Le {
        return Ok(None);
    }
    let (ScalarExpr::Column(rn_col), ScalarExpr::Literal(ScalarValue::Int64(k))) =
        (left.as_ref(), right.as_ref())
    else {
        return Ok(None);
    };
    if *k < 0 {
        return Ok(None);
    }

    // Optionally strip a passthrough projection (the derived table's SELECT
    // that re-exposes the aggregate columns + rn), mapping rn through it.
    let (wf_node, rn_in_wf) = match child.non_asap() {
        Some(NonASAPOp::Project { cols, child, .. }) => {
            let Some(ProjectItem {
                expr: ScalarExpr::Column(underlying),
                ..
            }) = cols.get(*rn_col)
            else {
                return Ok(None);
            };
            (child, *underlying)
        }
        _ => (child, *rn_col),
    };

    // The filtered column must be a `ROW_NUMBER()` window output — the single
    // column the SQLWindowFunc appends after its input, i.e. the last one.
    let Some(NonASAPOp::SQLWindowFunc {
        func: WindowFuncKind::RowNumber,
        partition_by,
        order_by,
        child: inner,
        ..
    }) = wf_node.non_asap()
    else {
        return Ok(None);
    };
    if order_by.is_empty() {
        return Ok(None);
    }
    if rn_in_wf != inner.schema.fields.len() {
        return Ok(None); // the predicate ranks some other column, not the row number
    }

    // Generic partitioned top-k. The window's ORDER BY keys are relative to
    // its input (`inner`), so they transfer directly to a `Sort` over `inner`.
    let sort = OperatorNode::non_asap_node(NonASAPOp::Sort {
        keys: order_by.clone(),
        partition_by: partition_by.clone(),
        child: Rc::clone(inner),
    })?;
    OperatorNode::non_asap_node(NonASAPOp::Limit {
        n: Some(*k as usize),
        offset: 0,
        partition_by: partition_by.clone(),
        child: sort,
    })
    .map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::scalar::ExprSemantics;
    use crate::pre_asap::query_expr::{
        ConcatDiscriminatorKey, GroupKeys, Source, WindowFrame, WindowFrameBound,
        WindowFrameOffset, WindowFrameUnits,
    };
    use crate::pre_asap::schema::{DataType, Field, Schema};

    fn node(op: NonASAPOp) -> Rc<OperatorNode> {
        Rc::new(OperatorNode::new(Operator::NonASAP(op)).expect("fixture derives a schema"))
    }

    fn scan() -> Rc<OperatorNode> {
        node(NonASAPOp::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Field::plain("ts", DataType::Timestamp, false),
                    Field::plain("service", DataType::Utf8, false),
                    Field::plain("value", DataType::Float64, false),
                ],
                0,
                vec![],
            ),
        })
    }

    fn aggregate(reduction: Reduction, agg: AggIntent, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
        node(NonASAPOp::Aggregate {
            reduction,
            measures: vec![agg],
            output_names: vec![],
            having: None,
            child,
        })
    }

    fn count() -> AggIntent {
        AggIntent::Count {
            accuracy: AccuracyTarget::Exact,
        }
    }

    /// `Aggregate{ by: [1], [Count] }` over the scan — output cols `[service, count]`.
    fn count_by_service() -> Rc<OperatorNode> {
        aggregate(Reduction::by(vec![1]), count(), scan())
    }

    fn key(col: usize, ascending: bool) -> Vec<SortKey> {
        vec![SortKey {
            expr: ScalarExpr::Column(col),
            ascending,
            nulls_first: false,
        }]
    }

    fn desc(col: usize) -> Vec<SortKey> {
        key(col, false)
    }

    fn limit(n: usize, offset: usize, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
        node(NonASAPOp::Limit {
            n: Some(n),
            offset,
            partition_by: GroupKeys::none(),
            child,
        })
    }

    fn sort(keys: Vec<SortKey>, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
        node(NonASAPOp::Sort {
            keys,
            partition_by: GroupKeys::none(),
            child,
        })
    }

    fn passthrough_project(child: Rc<OperatorNode>) -> Rc<OperatorNode> {
        node(NonASAPOp::Project {
            cols: vec![
                ProjectItem {
                    alias: None,
                    expr: ScalarExpr::Column(0),
                },
                ProjectItem {
                    alias: Some("c".into()),
                    expr: ScalarExpr::Column(1),
                },
            ],
            qualifier: None,
            child,
        })
    }

    fn concat(children: Vec<Rc<OperatorNode>>, key: Option<ConcatDiscriminatorKey>) -> Rc<OperatorNode> {
        node(NonASAPOp::Concat {
            children,
            discriminator_unique_key: key,
        })
    }

    fn measures(n: &OperatorNode) -> &[AggIntent] {
        match n.non_asap() {
            Some(NonASAPOp::Aggregate { measures, .. }) => measures,
            _ => &[],
        }
    }

    fn is_topk_over_count(n: &OperatorNode) -> bool {
        let Some(NonASAPOp::Aggregate { measures, child, .. }) = n.non_asap() else {
            return false;
        };
        matches!(measures.as_slice(), [AggIntent::TopK { k: 5, .. }])
            && matches!(self::measures(child), [AggIntent::Count { .. }])
    }

    #[test]
    fn promotes_count_ranked_limit_sort() {
        // Limit 5 { Sort DESC by count-col (1) { Aggregate[Count] by [1] } }.
        let q = limit(5, 0, sort(desc(1), count_by_service()));
        assert!(is_topk_over_count(&canonicalize(q).unwrap()));
    }

    #[test]
    fn promotes_through_a_passthrough_projection() {
        // …with a `SELECT service, count` projection between the Sort and the Agg.
        let q = limit(5, 0, sort(desc(1), passthrough_project(count_by_service())));
        assert!(is_topk_over_count(&canonicalize(q).unwrap()));
    }

    #[test]
    fn promoted_topk_reuses_the_inner_aggregate_node() {
        // The inner aggregate is untouched, so the rewrite shares it rather
        // than copying it.
        let agg = count_by_service();
        let out = canonicalize(limit(5, 0, sort(desc(1), Rc::clone(&agg)))).unwrap();
        let Some(NonASAPOp::Aggregate { child, .. }) = out.non_asap() else {
            panic!("expected TopK aggregate");
        };
        assert!(Rc::ptr_eq(child, &agg));
    }

    #[test]
    fn is_idempotent() {
        let q = limit(5, 0, sort(desc(1), count_by_service()));
        let once = canonicalize(q).unwrap();
        let twice = canonicalize(Rc::clone(&once)).unwrap();
        assert!(Rc::ptr_eq(&once, &twice), "canonicalize must be idempotent");
    }

    #[test]
    fn untouched_dag_is_returned_pointer_equal() {
        // Nothing here matches a rewrite: a Concat of two projections over
        // one shared aggregate. The root (and everything under it) must come
        // back as the same `Rc`.
        let agg = count_by_service();
        let q = concat(
            vec![
                passthrough_project(Rc::clone(&agg)),
                passthrough_project(Rc::clone(&agg)),
            ],
            None,
        );
        let out = canonicalize(Rc::clone(&q)).unwrap();
        assert!(Rc::ptr_eq(&out, &q));
    }

    #[test]
    fn rewritten_shared_subtree_stays_shared() {
        // One promotable subtree referenced twice is rewritten once.
        let branch = limit(5, 0, sort(desc(1), count_by_service()));
        let q = concat(vec![Rc::clone(&branch), Rc::clone(&branch)], None);
        let out = canonicalize(q).unwrap();
        let Some(NonASAPOp::Concat { children, .. }) = out.non_asap() else {
            panic!("expected Concat");
        };
        assert!(is_topk_over_count(&children[0]));
        assert!(Rc::ptr_eq(&children[0], &children[1]));
    }

    // ── Concat's discriminator_unique_key vs. canonicalize (issue #228) ──
    //
    // `discriminator_unique_key`'s `ColumnId`s were resolved against the
    // first branch's *pre-canonicalize* output schema. The key is dropped
    // whenever that branch's schema actually changed, and survives untouched
    // otherwise. Never guessed at.

    fn discriminator_key(n: &OperatorNode) -> &Option<ConcatDiscriminatorKey> {
        match n.non_asap() {
            Some(NonASAPOp::Concat {
                discriminator_unique_key,
                ..
            }) => discriminator_unique_key,
            _ => panic!("expected Concat"),
        }
    }

    #[test]
    fn concat_discriminator_key_survives_canonicalize_when_first_branch_is_unaffected() {
        // A plain `Aggregate` first branch matches neither rewrite trigger,
        // so its schema is identical before and after canonicalize.
        let q = concat(
            vec![count_by_service(), count_by_service()],
            Some(ConcatDiscriminatorKey::new(0, vec![1])),
        );
        let out = canonicalize(Rc::clone(&q)).unwrap();
        assert!(
            discriminator_key(&out).is_some(),
            "an untouched first branch's discriminator key must survive canonicalize"
        );
        assert!(Rc::ptr_eq(&out, &q));
    }

    #[test]
    fn concat_discriminator_key_is_dropped_when_first_branch_gets_rewritten() {
        // The first branch is exactly the heavy-hitter promotion trigger, so
        // canonicalize rewrites it to `Aggregate{TopK}`, whose own output is
        // a single column, not the original two (`[service, count]`). A key
        // resolved against the 2-column shape must not survive pointing at
        // the new 1-column schema.
        let promotable_branch = limit(5, 0, sort(desc(1), count_by_service()));
        let q = concat(
            vec![promotable_branch, count_by_service()],
            Some(ConcatDiscriminatorKey::new(0, vec![1])),
        );
        let out = canonicalize(q).unwrap();
        let Some(NonASAPOp::Concat {
            children,
            discriminator_unique_key,
        }) = out.non_asap()
        else {
            panic!("expected Concat");
        };
        assert!(
            is_topk_over_count(&children[0]),
            "the first branch is still promoted normally"
        );
        assert!(
            discriminator_unique_key.is_none(),
            "a stale discriminator key must be dropped, never silently kept wrong"
        );
        assert!(out.schema.unique_keys.is_empty(), "the dropped key leaves the schema");
    }

    #[test]
    fn does_not_promote_ascending_sort() {
        // Ascending = bottom-k: the Top-K ranking rule rejects it (needs
        // descending), so it stays a generic Sort+Limit (issue #38).
        let q = limit(5, 0, sort(key(1, true), count_by_service()));
        assert!(!is_topk_over_count(&canonicalize(q).unwrap()));
    }

    #[test]
    fn does_not_promote_with_offset() {
        let q = limit(5, 2, sort(desc(1), count_by_service()));
        assert!(!is_topk_over_count(&canonicalize(q).unwrap()));
    }

    #[test]
    fn does_not_promote_ranking_by_a_group_key() {
        // DESC by col 0 (the `service` group key), not the count → not a
        // frequency heavy-hitter.
        let q = limit(5, 0, sort(desc(0), count_by_service()));
        assert!(!is_topk_over_count(&canonicalize(q).unwrap()));
    }

    #[test]
    fn does_not_promote_when_limit_partition_disagrees_with_sort() {
        // A per-group Limit partitioned differently from its Sort is not the
        // top-k shape.
        let q = node(NonASAPOp::Limit {
            n: Some(5),
            offset: 0,
            partition_by: GroupKeys::by(vec![0]),
            child: sort(desc(1), count_by_service()),
        });
        assert!(!is_topk_over_count(&canonicalize(q).unwrap()));
    }

    #[test]
    fn promotes_sum_ranked_limit_sort_as_weighted_heavy_hitter() {
        let sum = aggregate(Reduction::by(vec![1]), AggIntent::Sum { col: None }, scan());
        let out = canonicalize(limit(5, 0, sort(desc(1), sum))).unwrap();
        let Some(NonASAPOp::Aggregate { measures, child, .. }) = out.non_asap() else {
            panic!("expected weighted TopK aggregate");
        };
        assert!(matches!(measures.as_slice(), [AggIntent::TopK { k: 5, .. }]));
        assert!(matches!(self::measures(child), [AggIntent::Sum { .. }]));
    }

    #[test]
    fn keeps_sum_over_counter_reduction_as_exact_value_ranking() {
        for counter in [AggIntent::Rate, AggIntent::Increase] {
            let derived = aggregate(Reduction::PerEntity, counter, scan());
            let sum = aggregate(Reduction::by(vec![1]), AggIntent::Sum { col: None }, derived);
            let out = canonicalize(limit(5, 0, sort(desc(1), sum))).unwrap();
            let Some(NonASAPOp::Limit { child, .. }) = out.non_asap() else {
                panic!("expected Limit, got {out:?}");
            };
            let Some(NonASAPOp::Sort { child, .. }) = child.non_asap() else {
                panic!("expected Sort under the Limit");
            };
            let Some(NonASAPOp::Aggregate { measures, child, .. }) = child.non_asap() else {
                panic!("expected Aggregate under the Sort");
            };
            assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
            assert!(matches!(child.non_asap(), Some(NonASAPOp::Aggregate { .. })));
        }
    }

    // ── ROW_NUMBER() partitioned top-k (issue #24) ──────────────────────────

    /// A scan with `[ts, service, region, value]`.
    fn scan4() -> Rc<OperatorNode> {
        node(NonASAPOp::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Field::plain("ts", DataType::Timestamp, false),
                    Field::plain("service", DataType::Utf8, false),
                    Field::plain("region", DataType::Utf8, false),
                    Field::plain("value", DataType::Float64, false),
                ],
                0,
                vec![],
            ),
        })
    }

    /// `Aggregate{ by: [1,2] (service, region), [agg] }` — output `[service,
    /// region, <agg>]` (3 cols), so a ROW_NUMBER over it appends `rn` at index 3.
    fn grouped(agg: AggIntent) -> Rc<OperatorNode> {
        aggregate(Reduction::by(vec![1, 2]), agg, scan4())
    }

    /// `ROW_NUMBER` ignores its frame clause; any concrete frame works.
    fn rownumber_frame() -> WindowFrame {
        WindowFrame {
            units: WindowFrameUnits::Rows,
            start_bound: WindowFrameBound::Preceding(WindowFrameOffset::Scalar(ScalarValue::Null)),
            end_bound: WindowFrameBound::Following(WindowFrameOffset::Scalar(ScalarValue::Null)),
        }
    }

    /// `SQLWindowFunc{ RowNumber, PARTITION BY region(2), ORDER BY col(2) DESC } { agg }`.
    fn rownumber_window(agg: Rc<OperatorNode>) -> Rc<OperatorNode> {
        node(NonASAPOp::SQLWindowFunc {
            func: WindowFuncKind::RowNumber,
            args: vec![],
            partition_by: GroupKeys::by(vec![2]), // region
            order_by: vec![SortKey {
                expr: ScalarExpr::Column(2), // the aggregate output column
                ascending: false,
                nulls_first: true,
            }],
            frame: Some(rownumber_frame()),
            output_name: "rn".into(),
            child: agg,
        })
    }

    /// `Filter{ col <= 5 } { child }`.
    fn filter_le_5(col: usize, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
        node(NonASAPOp::Filter {
            pred: Predicate(ScalarExpr::Compare {
                left: Box::new(ScalarExpr::Column(col)),
                op: CompareOpKind::Le,
                right: Box::new(ScalarExpr::Literal(ScalarValue::Int64(5))),
                semantics: ExprSemantics::Sql,
            }),
            child,
        })
    }

    /// `Filter{ rn(3) <= 5 } { ROW_NUMBER window { agg } }`.
    fn rownumber_topk(agg: Rc<OperatorNode>) -> Rc<OperatorNode> {
        filter_le_5(3, rownumber_window(agg))
    }

    #[test]
    fn rownumber_count_topk_becomes_a_partitioned_heavy_hitter() {
        // Count-ranked ROW_NUMBER top-k → outer TopK grouped by the partition
        // (region, col 2) over the explicit inner Count.
        let out = canonicalize(rownumber_topk(grouped(count()))).unwrap();
        let Some(NonASAPOp::Aggregate {
            reduction,
            measures,
            child,
            ..
        }) = out.non_asap()
        else {
            panic!("expected outer Aggregate([TopK]), got {out:?}");
        };
        let Reduction::Reduce(by) = reduction else {
            panic!("expected a Reduce grouping, got {reduction:?}");
        };
        assert!(matches!(measures.as_slice(), [AggIntent::TopK { k: 5, .. }]));
        assert_eq!(**by, vec![2], "outer TopK partitioned by region");
        assert!(matches!(self::measures(child), [AggIntent::Count { .. }]));
    }

    #[test]
    fn rownumber_avg_topk_becomes_a_partitioned_sort_limit() {
        // Avg-ranked (not a frequency heavy-hitter) → generic partitioned
        // top-k: Limit{5, partition_by: [region]}{ Sort{ partition_by: [region] } }.
        let out = canonicalize(rownumber_topk(grouped(AggIntent::Avg { col: None }))).unwrap();
        let Some(NonASAPOp::Limit {
            n,
            partition_by: limit_partition,
            child,
            ..
        }) = out.non_asap()
        else {
            panic!("expected a Limit, got {out:?}");
        };
        assert_eq!(*n, Some(5));
        assert_eq!(**limit_partition, vec![2], "limit applied per region");
        let Some(NonASAPOp::Sort {
            partition_by,
            child,
            ..
        }) = child.non_asap()
        else {
            panic!("expected a Sort under the Limit");
        };
        assert_eq!(**partition_by, vec![2], "partitioned by region");
        assert!(matches!(self::measures(child), [AggIntent::Avg { .. }]));
    }

    #[test]
    fn filter_on_a_non_rownumber_column_is_left_alone() {
        // `WHERE service_len <= 5` (col 0, not the rn window column) must not
        // be mistaken for a top-k.
        let q = filter_le_5(0, rownumber_window(grouped(count())));
        let out = canonicalize(Rc::clone(&q)).unwrap();
        assert!(Rc::ptr_eq(&out, &q), "left as the same Filter");
    }
}
