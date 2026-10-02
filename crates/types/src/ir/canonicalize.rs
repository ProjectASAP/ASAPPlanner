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
//!
//! ## Subquery lowering
//!
//! The SQL front end leaves `[NOT] EXISTS (…)`, `x IN (…)` and scalar
//! subqueries as the scalar variants `ScalarExpr::{Exists, InSubquery,
//! ScalarSubquery}` inside `Filter` predicates and `Project` items. This pass
//! lowers them to the join shapes the planner and the physical lowering
//! match on (`|l|` is the left input's column count; the join predicate of a
//! semi/anti join resolves against `left ++ right` even though its output is
//! the left's columns alone):
//!
//! ```text
//! Filter { EXISTS (s) ∧ rest }{ l }     → Filter { rest }{ Join { Semi, true, l, s } }
//! Filter { NOT EXISTS (s) ∧ rest }{ l } → Filter { rest }{ Join { Anti, true, l, s } }
//! Filter { x IN (s) ∧ rest }{ l }       → Filter { rest }{ Join { Semi, x = Column(|l|), l, s } }
//! Project { … (s) … }{ l }              → Project { … Column(|l|) … }{ Join { Cross, true, l, s } }
//! Filter { … (s) … }{ l }               → Project { l's columns }{
//!                                             Filter { … Column(|l|) … }{ Join { Cross, true, l, s } } }
//! ```
//!
//! `NOT IN (s)` is left as is: under SQL three-valued logic a NULL on either
//! side makes it UNKNOWN, which no anti-join reproduces (the front end rejects
//! it anyway). A lifted subquery is canonicalized like any other operator
//! input, so the result is a fixpoint of this pass.

use std::collections::HashMap;
use std::rc::Rc;

use super::node::{Operator, OperatorNode};
use super::non_asap::NonASAPOp;
use super::scalar::{ExprSemantics, Predicate, ProjectItem, ScalarExpr, SortKey};
use crate::pre_asap::agg_intent::{topk, AggIntent};
use crate::pre_asap::expr_ir::{CompareOpKind, ScalarValue};
use crate::pre_asap::query_expr::{JoinKind, QueryExprError, Reduction, WindowFuncKind};
use crate::pre_asap::schema::ColumnId;
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

    let current = apply_local_rules(current, memo)?;

    memo.insert(Rc::as_ptr(node), Rc::clone(&current));
    Ok(current)
}

type Memo = HashMap<*const OperatorNode, Rc<OperatorNode>>;

/// Apply the local rewrite rules at `node` (whose inputs are already
/// canonical) until none matches. The rules chain: a `ROW_NUMBER()`-
/// partitioned top-k rewrites to a `Limit{Sort}`, which the heavy-hitter
/// rule may then promote to an `Aggregate([TopK])`; a `Filter` with several
/// subquery conjuncts sheds one per round. Each rule strictly simplifies the
/// node (one fewer idiom, or one fewer subquery reference), so the loop
/// terminates.
fn apply_local_rules(
    mut current: Rc<OperatorNode>,
    memo: &mut Memo,
) -> Result<Rc<OperatorNode>, QueryExprError> {
    loop {
        let next = if let Some(next) = try_rewrite_rownumber_topk(&current)? {
            next
        } else if let Some(next) = try_promote_additive_top_ranking(&current)? {
            next
        } else if let Some(next) = try_lower_subquery_conjunct(&current, memo)? {
            next
        } else if let Some(next) = try_lower_scalar_subquery(&current, memo)? {
            next
        } else {
            break;
        };
        current = next;
    }
    Ok(current)
}

/// The direct **operator** inputs of a node — the relational skeleton only.
/// Operator nodes referenced from a scalar position (`ScalarSubquery`,
/// `Exists`, …) are not visited here: a subquery that the lowering rules
/// lift into a join is canonicalized at that point, and one they leave in
/// place (`NOT IN`, an `EXISTS` outside a `Filter` conjunct) stays as the
/// front end emitted it.
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

/// `Predicate(true)`: the unconditional join predicate the SQL front end
/// emits for an uncorrelated `EXISTS` and for a `CROSS JOIN`.
fn always_true() -> Predicate {
    Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true)))
}

/// Whether `conjunct` is one this pass lowers to a semi/anti join.
fn is_join_conjunct(conjunct: &ScalarExpr) -> bool {
    match conjunct {
        ScalarExpr::Exists { .. } => true,
        // An `IN` whose probe expression itself reads a scalar subquery is
        // lowered only after that subquery has been joined in by
        // `try_lower_scalar_subquery` (a `Join` predicate is not a place that
        // rule looks). `NOT IN` is never lowered — see the module docs.
        ScalarExpr::InSubquery {
            expr,
            negated: false,
            ..
        } => find_scalar_subquery(expr).is_none(),
        _ => false,
    }
}

/// Lower one `[NOT] EXISTS (s)` / `x IN (s)` conjunct of a `Filter` to the
/// semi-/anti-join the SQL front end used to emit directly. The remaining
/// conjuncts stay in an outer `Filter` over the join: a semi/anti join's
/// output schema is the left's, so their column ids are unchanged. One
/// conjunct per call; the fixpoint loop picks up the next.
fn try_lower_subquery_conjunct(
    node: &OperatorNode,
    memo: &mut Memo,
) -> Result<Option<Rc<OperatorNode>>, QueryExprError> {
    let Some(NonASAPOp::Filter {
        pred: Predicate(pred),
        child,
    }) = node.non_asap()
    else {
        return Ok(None);
    };
    let conjuncts = pred.conjuncts();
    let Some(idx) = conjuncts.iter().position(is_join_conjunct) else {
        return Ok(None);
    };
    let left_width = child.schema.fields.len();
    let (kind, subquery, join_pred) = match &conjuncts[idx] {
        // Uncorrelated by construction (the IR's `Exists` carries no outer
        // column references), so the join condition is unconditionally true.
        ScalarExpr::Exists { subquery, negated } => {
            let kind = if *negated {
                JoinKind::Anti
            } else {
                JoinKind::Semi
            };
            (kind, subquery, always_true())
        }
        // `x = <the subquery's single column>`, which sits right after the
        // left's columns in the `left ++ right` scope the predicate resolves
        // against.
        ScalarExpr::InSubquery { expr, subquery, .. } => (
            JoinKind::Semi,
            subquery,
            Predicate(ScalarExpr::Compare {
                left: expr.clone(),
                op: CompareOpKind::Eq,
                right: Box::new(ScalarExpr::Column(left_width)),
                semantics: ExprSemantics::Sql,
            }),
        ),
        _ => unreachable!("`is_join_conjunct` admitted a non-subquery conjunct"),
    };
    let join = OperatorNode::non_asap_node(NonASAPOp::Join {
        kind,
        pred: join_pred,
        left: Rc::clone(child),
        right: canon(subquery, memo)?,
    })?;
    let mut rest: Vec<ScalarExpr> = conjuncts
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != idx)
        .map(|(_, c)| c.clone())
        .collect();
    let out = match rest.len() {
        0 => join,
        1 => filter(rest.remove(0), join)?,
        _ => filter(ScalarExpr::BoolAnd(rest), join)?,
    };
    Ok(Some(out))
}

/// Lower one scalar subquery read by a `Project` item or a `Filter`
/// predicate: the owner reads it through a cross join against the subquery,
/// whose single column is appended after the left's (`Column(|left|)`), and
/// every occurrence of that subquery node in the owner is replaced by that
/// column reference. One subquery node per call; the fixpoint loop handles
/// the rest, each getting its own cross join further out (so earlier column
/// ids are never shifted). For a `Filter` the output schema is restored to
/// the left's columns by a positional `Project` over the result.
///
/// Not representable in the IR, and therefore not checked here: SQL raises
/// an error when a scalar subquery yields more than one row (the cross join
/// would duplicate the left's rows instead), and yields NULL when it yields
/// none (the cross join yields no rows instead).
fn try_lower_scalar_subquery(
    node: &OperatorNode,
    memo: &mut Memo,
) -> Result<Option<Rc<OperatorNode>>, QueryExprError> {
    match node.non_asap() {
        Some(NonASAPOp::Project {
            cols,
            qualifier,
            child,
        }) => {
            let Some(sub) = cols.iter().find_map(|c| find_scalar_subquery(&c.expr)) else {
                return Ok(None);
            };
            let column = child.schema.fields.len();
            let cols = cols
                .iter()
                .map(|c| ProjectItem {
                    alias: c.alias.clone(),
                    expr: replace_scalar_subquery(&c.expr, sub, column),
                })
                .collect();
            OperatorNode::non_asap_node(NonASAPOp::Project {
                cols,
                qualifier: qualifier.clone(),
                child: cross_join(child, sub, memo)?,
            })
            .map(Some)
        }
        Some(NonASAPOp::Filter {
            pred: Predicate(pred),
            child,
        }) => {
            let Some(sub) = find_scalar_subquery(pred) else {
                return Ok(None);
            };
            let column = child.schema.fields.len();
            let filtered = filter(
                replace_scalar_subquery(pred, sub, column),
                cross_join(child, sub, memo)?,
            )?;
            // The predicate may still hold subquery conjuncts (an `IN` whose
            // probe read this scalar subquery); lower them before the
            // positional projection hides the `Filter` from the loop.
            let filtered = apply_local_rules(filtered, memo)?;
            OperatorNode::non_asap_node(NonASAPOp::Project {
                cols: (0..column)
                    .map(|id| ProjectItem {
                        alias: None,
                        expr: ScalarExpr::Column(id),
                    })
                    .collect(),
                qualifier: None,
                child: filtered,
            })
            .map(Some)
        }
        _ => Ok(None),
    }
}

fn filter(pred: ScalarExpr, child: Rc<OperatorNode>) -> Result<Rc<OperatorNode>, QueryExprError> {
    OperatorNode::non_asap_node(NonASAPOp::Filter {
        pred: Predicate(pred),
        child,
    })
}

/// `left × subquery`, with the subquery canonicalized on the way in.
fn cross_join(
    left: &Rc<OperatorNode>,
    subquery: &Rc<OperatorNode>,
    memo: &mut Memo,
) -> Result<Rc<OperatorNode>, QueryExprError> {
    OperatorNode::non_asap_node(NonASAPOp::Join {
        kind: JoinKind::Cross,
        pred: always_true(),
        left: Rc::clone(left),
        right: canon(subquery, memo)?,
    })
}

/// The first `ScalarSubquery` node read by `expr` (pre-order over its scalar
/// children; referenced operator subgraphs are their own scope and are not
/// entered).
fn find_scalar_subquery(expr: &ScalarExpr) -> Option<&Rc<OperatorNode>> {
    if let ScalarExpr::ScalarSubquery(node) = expr {
        return Some(node);
    }
    expr.children().into_iter().find_map(find_scalar_subquery)
}

/// `expr` with every `ScalarSubquery(sub)` occurrence (the same node, by
/// pointer identity) replaced by `Column(column)`.
fn replace_scalar_subquery(
    expr: &ScalarExpr,
    sub: &Rc<OperatorNode>,
    column: ColumnId,
) -> ScalarExpr {
    rewrite_scalar(expr, &mut |e| match e {
        ScalarExpr::ScalarSubquery(node) if Rc::ptr_eq(node, sub) => {
            Some(ScalarExpr::Column(column))
        }
        _ => None,
    })
}

/// Rebuild `expr` top-down: where `f` returns `Some`, that replaces the
/// subtree (which is not descended into); elsewhere the node is rebuilt over
/// its rewritten scalar children. Operator nodes referenced from the tree
/// are a separate scope and are left as they are.
fn rewrite_scalar(
    expr: &ScalarExpr,
    f: &mut impl FnMut(&ScalarExpr) -> Option<ScalarExpr>,
) -> ScalarExpr {
    if let Some(replaced) = f(expr) {
        return replaced;
    }
    fn boxed(
        e: &ScalarExpr,
        f: &mut impl FnMut(&ScalarExpr) -> Option<ScalarExpr>,
    ) -> Box<ScalarExpr> {
        Box::new(rewrite_scalar(e, f))
    }
    fn each(
        es: &[ScalarExpr],
        f: &mut impl FnMut(&ScalarExpr) -> Option<ScalarExpr>,
    ) -> Vec<ScalarExpr> {
        es.iter().map(|e| rewrite_scalar(e, f)).collect()
    }
    match expr {
        ScalarExpr::Column(_)
        | ScalarExpr::Literal(_)
        | ScalarExpr::CurrentTimestamp
        | ScalarExpr::EvalTimestamp
        | ScalarExpr::PromqlScalarFromVector(_)
        | ScalarExpr::ScalarSubquery(_)
        | ScalarExpr::Exists { .. } => expr.clone(),
        ScalarExpr::Negative { expr, semantics } => ScalarExpr::Negative {
            expr: boxed(expr, f),
            semantics: *semantics,
        },
        ScalarExpr::Compare {
            left,
            op,
            right,
            semantics,
        } => ScalarExpr::Compare {
            left: boxed(left, f),
            op: op.clone(),
            right: boxed(right, f),
            semantics: *semantics,
        },
        ScalarExpr::BoolAnd(parts) => ScalarExpr::BoolAnd(each(parts, f)),
        ScalarExpr::BoolOr(parts) => ScalarExpr::BoolOr(each(parts, f)),
        ScalarExpr::Not(e) => ScalarExpr::Not(boxed(e, f)),
        ScalarExpr::IsNull(e) => ScalarExpr::IsNull(boxed(e, f)),
        ScalarExpr::IsNotNull(e) => ScalarExpr::IsNotNull(boxed(e, f)),
        ScalarExpr::Cast { expr, to, try_cast } => ScalarExpr::Cast {
            expr: boxed(expr, f),
            to: to.clone(),
            try_cast: *try_cast,
        },
        ScalarExpr::InList {
            expr,
            list,
            negated,
        } => ScalarExpr::InList {
            expr: boxed(expr, f),
            list: each(list, f),
            negated: *negated,
        },
        ScalarExpr::FunctionCall { name, args } => ScalarExpr::FunctionCall {
            name: name.clone(),
            args: each(args, f),
        },
        ScalarExpr::Arithmetic {
            op,
            left,
            right,
            semantics,
        } => ScalarExpr::Arithmetic {
            op: op.clone(),
            left: boxed(left, f),
            right: boxed(right, f),
            semantics: *semantics,
        },
        ScalarExpr::Case {
            operand,
            branches,
            else_expr,
        } => ScalarExpr::Case {
            operand: operand.as_ref().map(|e| boxed(e, f)),
            branches: branches
                .iter()
                .map(|(w, t)| (rewrite_scalar(w, f), rewrite_scalar(t, f)))
                .collect(),
            else_expr: else_expr.as_ref().map(|e| boxed(e, f)),
        },
        ScalarExpr::InSubquery {
            expr,
            subquery,
            negated,
        } => ScalarExpr::InSubquery {
            expr: boxed(expr, f),
            subquery: Rc::clone(subquery),
            negated: *negated,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    // ── Subquery lowering ───────────────────────────────────────────────────

    fn filter_of(pred: ScalarExpr, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
        node(NonASAPOp::Filter {
            pred: Predicate(pred),
            child,
        })
    }

    /// `SELECT service FROM scan` — a one-column subquery.
    fn one_column_subquery() -> Rc<OperatorNode> {
        node(NonASAPOp::Project {
            cols: vec![ProjectItem {
                alias: None,
                expr: ScalarExpr::Column(1),
            }],
            qualifier: None,
            child: scan(),
        })
    }

    fn exists(subquery: Rc<OperatorNode>, negated: bool) -> ScalarExpr {
        ScalarExpr::Exists { subquery, negated }
    }

    fn in_subquery(expr: ScalarExpr, subquery: Rc<OperatorNode>, negated: bool) -> ScalarExpr {
        ScalarExpr::InSubquery {
            expr: Box::new(expr),
            subquery,
            negated,
        }
    }

    /// `value(2) > 1`.
    fn value_gt_1() -> ScalarExpr {
        ScalarExpr::Compare {
            left: Box::new(ScalarExpr::Column(2)),
            op: CompareOpKind::Gt,
            right: Box::new(ScalarExpr::Literal(ScalarValue::Int64(1))),
            semantics: ExprSemantics::Sql,
        }
    }

    fn literal_true() -> ScalarExpr {
        ScalarExpr::Literal(ScalarValue::Boolean(true))
    }

    fn join_parts(
        n: &OperatorNode,
    ) -> (JoinKind, &ScalarExpr, &Rc<OperatorNode>, &Rc<OperatorNode>) {
        match n.non_asap() {
            Some(NonASAPOp::Join {
                kind,
                pred: Predicate(pred),
                left,
                right,
            }) => (kind.clone(), pred, left, right),
            _ => panic!("expected a Join, got {n:?}"),
        }
    }

    fn assert_idempotent(once: &Rc<OperatorNode>) {
        let twice = canonicalize(Rc::clone(once)).unwrap();
        assert!(Rc::ptr_eq(once, &twice), "canonicalize must be idempotent");
    }

    #[test]
    fn exists_filter_becomes_semi_join() {
        let (left, sub) = (scan(), one_column_subquery());
        let q = filter_of(exists(Rc::clone(&sub), false), Rc::clone(&left));
        let out = canonicalize(q).unwrap();
        let (kind, pred, l, r) = join_parts(&out);
        assert_eq!(kind, JoinKind::Semi);
        assert_eq!(*pred, literal_true());
        assert!(Rc::ptr_eq(l, &left) && Rc::ptr_eq(r, &sub));
        assert_eq!(
            out.schema.fields, left.schema.fields,
            "a semi join outputs the left's columns"
        );
        assert_idempotent(&out);
    }

    #[test]
    fn not_exists_becomes_anti_join() {
        let (left, sub) = (scan(), one_column_subquery());
        let q = filter_of(exists(Rc::clone(&sub), true), Rc::clone(&left));
        let out = canonicalize(q).unwrap();
        let (kind, pred, l, r) = join_parts(&out);
        assert_eq!(kind, JoinKind::Anti);
        assert_eq!(*pred, literal_true());
        assert!(Rc::ptr_eq(l, &left) && Rc::ptr_eq(r, &sub));
        assert_idempotent(&out);
    }

    #[test]
    fn in_subquery_becomes_semi_join_on_the_subquery_column() {
        // `WHERE service IN (SELECT service …)` over a 3-column left: the
        // subquery's column is `Column(3)` in the `left ++ right` scope.
        let (left, sub) = (scan(), one_column_subquery());
        let q = filter_of(
            in_subquery(ScalarExpr::Column(1), Rc::clone(&sub), false),
            Rc::clone(&left),
        );
        let out = canonicalize(q).unwrap();
        let (kind, pred, l, r) = join_parts(&out);
        assert_eq!(kind, JoinKind::Semi);
        assert_eq!(
            *pred,
            ScalarExpr::Compare {
                left: Box::new(ScalarExpr::Column(1)),
                op: CompareOpKind::Eq,
                right: Box::new(ScalarExpr::Column(3)),
                semantics: ExprSemantics::Sql,
            }
        );
        assert!(Rc::ptr_eq(l, &left) && Rc::ptr_eq(r, &sub));
        assert_eq!(
            out.schema.fields, left.schema.fields,
            "a semi join outputs the left's columns"
        );
        assert_idempotent(&out);
    }

    #[test]
    fn exists_with_other_conjuncts_keeps_an_outer_filter() {
        // `WHERE value > 1 AND EXISTS (…)` → Filter{ value > 1 }{ Semi }.
        let (left, sub) = (scan(), one_column_subquery());
        let q = filter_of(
            ScalarExpr::BoolAnd(vec![value_gt_1(), exists(Rc::clone(&sub), false)]),
            Rc::clone(&left),
        );
        let out = canonicalize(q).unwrap();
        let Some(NonASAPOp::Filter {
            pred: Predicate(pred),
            child,
        }) = out.non_asap()
        else {
            panic!("expected an outer Filter, got {out:?}");
        };
        assert_eq!(*pred, value_gt_1());
        let (kind, _, l, r) = join_parts(child);
        assert_eq!(kind, JoinKind::Semi);
        assert!(Rc::ptr_eq(l, &left) && Rc::ptr_eq(r, &sub));
        assert_idempotent(&out);
    }

    #[test]
    fn two_subquery_conjuncts_become_nested_joins() {
        // `WHERE EXISTS (a) AND service NOT EXISTS (b) AND value > 1` sheds
        // one conjunct per round: Filter{ value > 1 }{ Anti{ Semi{ l, a }, b } }.
        let (left, a, b) = (scan(), one_column_subquery(), one_column_subquery());
        let q = filter_of(
            ScalarExpr::BoolAnd(vec![
                exists(Rc::clone(&a), false),
                exists(Rc::clone(&b), true),
                value_gt_1(),
            ]),
            Rc::clone(&left),
        );
        let out = canonicalize(q).unwrap();
        let Some(NonASAPOp::Filter {
            pred: Predicate(pred),
            child,
        }) = out.non_asap()
        else {
            panic!("expected an outer Filter, got {out:?}");
        };
        assert_eq!(*pred, value_gt_1());
        let (kind, _, inner, r) = join_parts(child);
        assert_eq!(kind, JoinKind::Anti);
        assert!(Rc::ptr_eq(r, &b));
        let (kind, _, l, r) = join_parts(inner);
        assert_eq!(kind, JoinKind::Semi);
        assert!(Rc::ptr_eq(l, &left) && Rc::ptr_eq(r, &a));
        assert_idempotent(&out);
    }

    #[test]
    fn scalar_subquery_in_projection_becomes_cross_join() {
        // `SELECT service, value - (SELECT …)` → the subquery's column is
        // `Column(3)` after the cross join.
        let (left, sub) = (scan(), one_column_subquery());
        let minus = |rhs: ScalarExpr| ScalarExpr::Arithmetic {
            op: crate::pre_asap::expr_ir::ArithmeticOpKind::Sub,
            left: Box::new(ScalarExpr::Column(2)),
            right: Box::new(rhs),
            semantics: ExprSemantics::Sql,
        };
        let q = node(NonASAPOp::Project {
            cols: vec![
                ProjectItem {
                    alias: None,
                    expr: ScalarExpr::Column(1),
                },
                ProjectItem {
                    alias: Some("delta".into()),
                    expr: minus(ScalarExpr::ScalarSubquery(Rc::clone(&sub))),
                },
            ],
            qualifier: None,
            child: Rc::clone(&left),
        });
        let out = canonicalize(q).unwrap();
        let Some(NonASAPOp::Project { cols, child, .. }) = out.non_asap() else {
            panic!("expected a Project, got {out:?}");
        };
        assert_eq!(cols[0].expr, ScalarExpr::Column(1));
        assert_eq!(cols[1].alias.as_deref(), Some("delta"));
        assert_eq!(cols[1].expr, minus(ScalarExpr::Column(3)));
        let (kind, pred, l, r) = join_parts(child);
        assert_eq!(kind, JoinKind::Cross);
        assert_eq!(*pred, literal_true());
        assert!(Rc::ptr_eq(l, &left) && Rc::ptr_eq(r, &sub));
        assert_eq!(out.schema.fields.len(), 2);
        assert_idempotent(&out);
    }

    #[test]
    fn each_scalar_subquery_gets_its_own_cross_join() {
        // Two distinct subqueries: the first lands at Column(3), the second
        // (joined further out) at Column(4); the left's ids never shift.
        let (left, a, b) = (scan(), one_column_subquery(), one_column_subquery());
        let q = node(NonASAPOp::Project {
            cols: vec![
                ProjectItem {
                    alias: None,
                    expr: ScalarExpr::ScalarSubquery(Rc::clone(&a)),
                },
                ProjectItem {
                    alias: None,
                    expr: ScalarExpr::ScalarSubquery(Rc::clone(&b)),
                },
                ProjectItem {
                    alias: None,
                    expr: ScalarExpr::Column(2),
                },
            ],
            qualifier: None,
            child: Rc::clone(&left),
        });
        let out = canonicalize(q).unwrap();
        let Some(NonASAPOp::Project { cols, child, .. }) = out.non_asap() else {
            panic!("expected a Project, got {out:?}");
        };
        let exprs: Vec<_> = cols.iter().map(|c| c.expr.clone()).collect();
        assert_eq!(
            exprs,
            vec![
                ScalarExpr::Column(3),
                ScalarExpr::Column(4),
                ScalarExpr::Column(2)
            ]
        );
        let (kind, _, inner, r) = join_parts(child);
        assert_eq!(kind, JoinKind::Cross);
        assert!(Rc::ptr_eq(r, &b));
        let (kind, _, l, r) = join_parts(inner);
        assert_eq!(kind, JoinKind::Cross);
        assert!(Rc::ptr_eq(l, &left) && Rc::ptr_eq(r, &a));
        assert_idempotent(&out);
    }

    #[test]
    fn scalar_subquery_in_filter_restores_the_left_schema() {
        // `WHERE value > (SELECT …)` → Project{ left's cols }{ Filter{ value >
        // Column(3) }{ Cross{ left, sub } } }.
        let (left, sub) = (scan(), one_column_subquery());
        let gt = |rhs: ScalarExpr| ScalarExpr::Compare {
            left: Box::new(ScalarExpr::Column(2)),
            op: CompareOpKind::Gt,
            right: Box::new(rhs),
            semantics: ExprSemantics::Sql,
        };
        let q = filter_of(
            gt(ScalarExpr::ScalarSubquery(Rc::clone(&sub))),
            Rc::clone(&left),
        );
        let out = canonicalize(q).unwrap();
        let Some(NonASAPOp::Project { cols, child, .. }) = out.non_asap() else {
            panic!("expected a restoring Project, got {out:?}");
        };
        let exprs: Vec<_> = cols.iter().map(|c| c.expr.clone()).collect();
        assert_eq!(
            exprs,
            vec![
                ScalarExpr::Column(0),
                ScalarExpr::Column(1),
                ScalarExpr::Column(2)
            ]
        );
        let names: Vec<_> = out.schema.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["ts", "service", "value"]);
        let Some(NonASAPOp::Filter {
            pred: Predicate(pred),
            child,
        }) = child.non_asap()
        else {
            panic!("expected a Filter under the Project");
        };
        assert_eq!(*pred, gt(ScalarExpr::Column(3)));
        let (kind, _, l, r) = join_parts(child);
        assert_eq!(kind, JoinKind::Cross);
        assert!(Rc::ptr_eq(l, &left) && Rc::ptr_eq(r, &sub));
        assert_idempotent(&out);
    }

    #[test]
    fn not_in_subquery_is_left_alone() {
        let q = filter_of(
            in_subquery(ScalarExpr::Column(1), one_column_subquery(), true),
            scan(),
        );
        let out = canonicalize(Rc::clone(&q)).unwrap();
        assert!(Rc::ptr_eq(&out, &q), "NOT IN keeps its Filter");
        assert_idempotent(&out);
    }

    #[test]
    fn lifted_subquery_is_canonicalized() {
        // The subquery is itself a promotable heavy-hitter; once lifted into
        // the join it is canonical, so a second pass finds nothing to do.
        let sub = limit(5, 0, sort(desc(1), count_by_service()));
        let q = filter_of(exists(sub, false), scan());
        let out = canonicalize(q).unwrap();
        let (_, _, _, r) = join_parts(&out);
        assert!(is_topk_over_count(r));
        assert_idempotent(&out);
    }
}
