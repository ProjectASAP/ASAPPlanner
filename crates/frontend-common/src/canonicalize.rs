//! Post-lowering canonicalization of the operator DAG.
//!
//! Rewrites equivalent spellings of a query into one shape, so that common
//! sub-DAG sharing (Stage 0 hash-consing, `cse` in `asap_logical_asap_optimization`) can match
//! them by structural equality, whichever front end produced them.
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
//! retain Sort + Limit. The promotion changes the node's columns, so it
//! applies only where no consumer reads them by position: at the root or
//! in a branch of a `Concat` that is not read so either.
//!
//! SQL's per-partition top-k, `WHERE rn <= k` over `ROW_NUMBER() OVER
//! (PARTITION BY p ORDER BY o)` under a SELECT list that drops `rn`, is
//! first rewritten to the per-partition `Limit{k}{Sort{o}}` PromQL `topk by
//! (p)` lowers to, which the promotion then applies to as to any other.
//!
//! ## Subquery lowering
//!
//! EXISTS/NOT EXISTS and positive IN filter conjuncts may use semi/anti joins.
//! Scalar subqueries remain explicit: a cross join does not preserve their
//! zero-row NULL or multiple-row error semantics. All scalar plan references
//! participate in DAG traversal and canonicalization.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use asap_types::ir::operator::agg_intent::{topk, AggIntent};
use asap_types::ir::operator::node::{Operator, OperatorNode};
use asap_types::ir::operator::non_asap::NonASAPOp;
use asap_types::ir::operator::operator_properties::{
    GroupKeys, JoinKind, Reduction, WindowFuncKind,
};
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::scalar::expr_ir::{CompareOpKind, ScalarValue};
use asap_types::ir::scalar::{ExprSemantics, Predicate, ProjectItem, ScalarExpr, SortKey};
use asap_types::ir::SchemaDerivationError;

/// Rewrite the DAG under `root` into its canonical form (bottom-up).
/// Idempotent: an already-canonical DAG comes back as the same `Rc`. Only
/// nodes that change (or whose inputs change) are rebuilt; every untouched
/// sub-DAG keeps its pointer identity, and a shared sub-DAG that is rewritten
/// stays shared.
pub(crate) fn canonicalize(
    root: Rc<OperatorNode>,
) -> Result<Rc<OperatorNode>, SchemaDerivationError> {
    canon(
        &root,
        &mut NodeToCanonicalNodeCache::new(),
        &read_by_position(&root),
    )
}

/// The nodes under `root` some consumer reads by column position: every
/// input of an operator other than `Concat`, and the branches of a `Concat`
/// itself read so (a branch gives the whole output of a `Concat` that is
/// the answer). A rewrite that changes a node's columns, as heavy-hitter
/// promotion does, applies only to a node outside this set, whose columns
/// are the query's answer: rewritten under a positional reader, a column
/// index would name another column.
fn read_by_position(root: &Rc<OperatorNode>) -> HashSet<*const OperatorNode> {
    let nodes = OperatorNode::reachable(root);
    let is_concat = |node: &OperatorNode| matches!(node.non_asap(), Some(NonASAPOp::Concat { .. }));
    let mut read: HashSet<*const OperatorNode> = nodes
        .iter()
        .filter(|node| !is_concat(node))
        .flat_map(|node| node.children().into_iter().map(Rc::as_ptr))
        .collect();
    // A `Concat` read by position passes that on to its branches, through
    // nested `Concat`s too.
    let mut pending: Vec<&Rc<OperatorNode>> = nodes
        .iter()
        .filter(|node| is_concat(node) && read.contains(&Rc::as_ptr(node)))
        .collect();
    while let Some(concat) = pending.pop() {
        for branch in concat.children() {
            if read.insert(Rc::as_ptr(branch)) && is_concat(branch) {
                pending.push(branch);
            }
        }
    }
    read
}

/// Already-canonicalized nodes, by pointer. The recursion needs this table so
/// a shared sub-DAG is rewritten once and stays shared; `canonicalize` hides
/// it from callers.
type NodeToCanonicalNodeCache = HashMap<*const OperatorNode, Rc<OperatorNode>>;

fn canon(
    node: &Rc<OperatorNode>,
    cache: &mut NodeToCanonicalNodeCache,
    read_by_position: &HashSet<*const OperatorNode>,
) -> Result<Rc<OperatorNode>, SchemaDerivationError> {
    if let Some(done) = cache.get(&Rc::as_ptr(node)) {
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

    // Bottom-up: canonicalize every child (operator inputs and the nodes read
    // by scalar expressions) before matching at this node, so an inner
    // heavy-hitter is promoted before an enclosing rewrite inspects it.
    let mut rebuilt: Vec<(*const OperatorNode, Rc<OperatorNode>)> = Vec::new();
    let mut changed = false;
    for child in node.operator.children() {
        let new = canon(child, cache, read_by_position)?;
        changed |= !Rc::ptr_eq(&new, child);
        rebuilt.push((Rc::as_ptr(child), new));
    }
    let mut current = if changed {
        let rebuilt_child = |c: &Rc<OperatorNode>| {
            rebuilt
                .iter()
                .find(|(ptr, _)| *ptr == Rc::as_ptr(c))
                .map_or_else(|| Rc::clone(c), |(_, new)| Rc::clone(new))
        };
        Rc::new(node.with_new_children(rebuilt_child)?)
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
            current =
                OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Concat {
                    children: children.clone(),
                    discriminator_unique_key: None,
                }))?;
        }
    }

    let columns_may_change = !read_by_position.contains(&Rc::as_ptr(node));
    let current = apply_local_rules(current, columns_may_change, cache, read_by_position)?;

    cache.insert(Rc::as_ptr(node), Rc::clone(&current));
    Ok(current)
}

/// Apply the local rewrite rules at `node` (whose inputs are already
/// canonical) until none matches. A `Filter` with several subquery
/// conjuncts sheds one per round. Each rule removes one window, one
/// `Limit{Sort}` idiom or one subquery conjunct, so the loop terminates.
/// Heavy-hitter promotion changes the node's columns, so it applies only
/// when `columns_may_change` (no consumer reads them by position).
fn apply_local_rules(
    mut current: Rc<OperatorNode>,
    columns_may_change: bool,
    cache: &mut NodeToCanonicalNodeCache,
    read_by_position: &HashSet<*const OperatorNode>,
) -> Result<Rc<OperatorNode>, SchemaDerivationError> {
    loop {
        let promoted = || match columns_may_change {
            true => try_promote_additive_top_ranking(&current),
            false => Ok(None),
        };
        let next = if let Some(next) = try_rewrite_row_number_top_k(&current)? {
            next
        } else if let Some(next) = promoted()? {
            next
        } else if let Some(next) = try_lower_subquery_conjunct(&current, cache, read_by_position)? {
            next
        } else {
            break;
        };
        current = next;
    }
    Ok(current)
}

/// Recognise an additive-ranked
/// `Limit { Sort { [Project] Aggregate([Count | Sum]) } }` and rewrite it to
/// the canonical heavy-hitter `Aggregate([TopK])` over the explicit inner
/// aggregate. Returns `None` when the shape does not match.
fn try_promote_additive_top_ranking(
    node: &OperatorNode,
) -> Result<Option<Rc<OperatorNode>>, SchemaDerivationError> {
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
    // The `Limit` must keep k rows per the `Sort`'s partition: the ranking's
    // partition is what the outer `TopK` groups by. A global `Limit` over a
    // partitioned `Sort` keeps k rows overall, not k per group.
    if limit_partition != partition_by {
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
    // projection (a bare-column SELECT list). Map the sort key and the
    // partition through the projection to the aggregate's own output columns.
    let (agg_node, ranked_col, partition) = match sort_child.non_asap() {
        Some(NonASAPOp::Project { cols, child, .. }) => {
            let underlying = |column: usize| match cols.get(column) {
                Some(ProjectItem {
                    expr: ScalarExpr::Column(underlying),
                    ..
                }) => Some(*underlying),
                _ => None,
            };
            let Some(ranked_col) = underlying(*sort_col) else {
                return Ok(None);
            };
            let Some(partition) = partition_by.iter().map(|&c| underlying(c)).collect() else {
                return Ok(None);
            };
            (child, ranked_col, partition)
        }
        _ => (sort_child, *sort_col, partition_by.to_vec()),
    };

    // Exactly one aggregate, ranked by *its* output column — the measure sits
    // at index `by.len()` (after the group keys). A `PerEntity` reduction has
    // no `by` to rank a measure against, so it is a non-match.
    let Some(NonASAPOp::Aggregate {
        reduction,
        measures,
        filters,
        child: aggregate_child,
        ..
    }) = agg_node.non_asap()
    else {
        return Ok(None);
    };
    let Reduction::Reduce(by) = reduction else {
        return Ok(None);
    };
    // A heavy-hitter sketch ranks the raw update stream; a filtered measure
    // counts only part of it, and no binding rule applies the filter (#466).
    if asap_types::ir::operator::non_asap::any_measure_filtered(filters) {
        return Ok(None);
    }
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
        && matches!(
            aggregate_child.non_asap(),
            Some(NonASAPOp::Aggregate { .. })
        )
    {
        return Ok(None);
    }
    // Count ranks unit updates; a direct Sum ranks weighted updates.
    let accuracy = match ranked_agg {
        AggIntent::Count { accuracy } => accuracy.clone(),
        AggIntent::Sum { .. } => AccuracyTarget::exact(),
        _ => unreachable!("additive ranking gate admitted a non-additive measure"),
    };

    // Outer heavy-hitter `TopK`, grouped by the ranking's partition (empty for
    // a global `ORDER BY … LIMIT k`; the `by` labels for a partitioned `topk
    // by`), over the unchanged inner additive aggregate.
    OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Aggregate {
        reduction: Reduction::by(partition),
        measures: vec![AggIntent::TopK { k: *k, accuracy }],
        output_names: Vec::new(),
        filters: vec![],
        having: None,
        child: Rc::clone(agg_node),
    }))
    .map(Some)
}

/// Recognise the SQL per-partition top-k idiom
///
/// ```text
/// Project{P} { Filter{ rn <= k } { [Project{Q}] {
///     SQLWindowFunc{ RowNumber, PARTITION BY p, ORDER BY o } { X } } } }
/// ```
///
/// where the SELECT list `P` drops `rn` and keeps only columns of `X`, and
/// rewrite it to the per-partition `Limit{k}{Sort{o}}` over `X` that PromQL
/// `topk by (p)` lowers to. Two shapes, both with `P`'s columns:
///
/// - `P` keeps the partition and ordering columns: `Limit{k, p}{Sort{o,
///   p}{Project{P}{X}}}`, which the heavy-hitter rule then promotes to
///   `Aggregate{ Reduce(p), [TopK{k}] }` when `o` is one descending count
///   or sum of an aggregate `X`.
/// - otherwise: `Project{P}{Limit{k, p}{Sort{o, p}{X}}}`.
///
/// `rn < k` is `rn <= k - 1`, and `rn = 1` is `rn <= 1`. Rows tied on `o` at the k-th place are cut
/// arbitrarily, as by `ROW_NUMBER` itself. `None` when the shape does not
/// match: a visible `rn`, another window function or predicate, a computed
/// SELECT item.
fn try_rewrite_row_number_top_k(
    node: &OperatorNode,
) -> Result<Option<Rc<OperatorNode>>, SchemaDerivationError> {
    let Some(NonASAPOp::Project {
        cols: select_list,
        qualifier,
        child: filter,
    }) = node.non_asap()
    else {
        return Ok(None);
    };
    let Some(NonASAPOp::Filter {
        pred: Predicate(ScalarExpr::Compare {
            left, op, right, ..
        }),
        child: filter_child,
    }) = filter.non_asap()
    else {
        return Ok(None);
    };
    let (ScalarExpr::Column(rn_column), ScalarExpr::Literal(ScalarValue::Int64(bound))) =
        (left.as_ref(), right.as_ref())
    else {
        return Ok(None);
    };
    let rows_per_partition = match op {
        CompareOpKind::Le => *bound,
        CompareOpKind::Lt => bound.saturating_sub(1),
        // `rn = 1`: the top row per partition.
        CompareOpKind::Eq if *bound == 1 => 1,
        _ => return Ok(None),
    };
    let Ok(rows_per_partition @ 1..) = usize::try_from(rows_per_partition) else {
        return Ok(None);
    };
    // The derived table's SELECT between the filter and the window, if any:
    // each column the filter or `P` reads must pass a window column through.
    let (window, through_derived_table): (&Rc<OperatorNode>, Option<&[ProjectItem]>) =
        match filter_child.non_asap() {
            Some(NonASAPOp::Project { cols, child, .. }) => (child, Some(cols)),
            _ => (filter_child, None),
        };
    let window_column = |column: usize| match through_derived_table {
        Some(cols) => match cols.get(column) {
            Some(ProjectItem {
                expr: ScalarExpr::Column(underlying),
                ..
            }) => Some(*underlying),
            _ => None,
        },
        None => Some(column),
    };
    let Some(NonASAPOp::SQLWindowFunc {
        func: WindowFuncKind::RowNumber,
        args,
        partition_by,
        order_by,
        child: ranked,
        ..
    }) = window.non_asap()
    else {
        return Ok(None);
    };
    // `ROW_NUMBER` appends its column after its input's.
    let row_number = ranked.schema.fields.len();
    if !args.is_empty() || order_by.is_empty() || window_column(*rn_column) != Some(row_number) {
        return Ok(None);
    }
    // Each SELECT item as a column of the window's input, under its output
    // name; `rn` itself is not one.
    let mut kept = Vec::with_capacity(select_list.len());
    for (item, field) in select_list.iter().zip(&node.schema.fields) {
        let ScalarExpr::Column(column) = item.expr else {
            return Ok(None);
        };
        match window_column(column) {
            Some(underlying) if underlying < row_number => kept.push(ProjectItem {
                alias: Some(field.name.clone()),
                expr: ScalarExpr::Column(underlying),
            }),
            _ => return Ok(None),
        }
    }
    let new = |operator: NonASAPOp| OperatorNode::new_shared(Operator::NonASAP(operator));
    let limit = |partition_by: GroupKeys, keys: Vec<SortKey>, child: Rc<OperatorNode>| {
        let sorted = new(NonASAPOp::Sort {
            keys,
            partition_by: partition_by.clone(),
            child,
        })?;
        new(NonASAPOp::Limit {
            n: Some(rows_per_partition),
            offset: 0,
            partition_by,
            child: sorted,
        })
    };
    // A column of `X` as a column of `Project{P}{X}`, if `P` keeps it.
    let position_in_kept = |column: usize| {
        kept.iter()
            .position(|item| item.expr == ScalarExpr::Column(column))
    };
    let partition_in_kept: Option<Vec<usize>> =
        partition_by.iter().map(|&c| position_in_kept(c)).collect();
    let order_in_kept: Option<Vec<SortKey>> = order_by
        .iter()
        .map(|key| match key.expr {
            ScalarExpr::Column(column) => position_in_kept(column).map(|position| SortKey {
                expr: ScalarExpr::Column(position),
                ..key.clone()
            }),
            _ => None,
        })
        .collect();
    let rewritten = match (partition_in_kept, order_in_kept) {
        (Some(partition), Some(order)) if !partition_by.is_without() => {
            let selected = new(NonASAPOp::Project {
                cols: kept,
                qualifier: qualifier.clone(),
                child: Rc::clone(ranked),
            })?;
            limit(GroupKeys::by(partition), order, selected)?
        }
        _ => new(NonASAPOp::Project {
            cols: kept,
            qualifier: qualifier.clone(),
            child: limit(partition_by.clone(), order_by.clone(), Rc::clone(ranked))?,
        })?,
    };
    Ok(Some(rewritten))
}

/// `Predicate(true)`: the unconditional join predicate the SQL front end
/// emits for an uncorrelated `EXISTS` and for a `CROSS JOIN`.
fn always_true() -> Predicate {
    Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true)))
}

/// True for the `Filter` conjuncts that `try_lower_subquery_conjunct` turns
/// into a join: `[NOT] EXISTS (s)` and a positive `x IN (s)`. `NOT IN` is
/// left alone (see the module docs), and so is an `IN` whose probe `x`
/// contains a scalar subquery, which would otherwise be moved into the join
/// predicate.
fn is_join_conjunct(conjunct: &ScalarExpr) -> bool {
    match conjunct {
        ScalarExpr::Exists { .. } => true,
        ScalarExpr::InSubquery {
            expr,
            negated: false,
            ..
        } => !contains_scalar_subquery(expr),
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
    cache: &mut NodeToCanonicalNodeCache,
    read_by_position: &HashSet<*const OperatorNode>,
) -> Result<Option<Rc<OperatorNode>>, SchemaDerivationError> {
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
    let join = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Join {
        kind,
        pred: join_pred,
        left: Rc::clone(child),
        right: canon(subquery, cache, read_by_position)?,
    }))?;
    let mut rest: Vec<ScalarExpr> = conjuncts
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != idx)
        .map(|(_, c)| c.clone())
        .collect();
    let rest = match rest.len() {
        0 => return Ok(Some(join)),
        1 => rest.remove(0),
        _ => ScalarExpr::BoolAnd(rest),
    };
    let out = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Filter {
        pred: Predicate(rest),
        child: join,
    }))?;
    Ok(Some(out))
}

/// Whether `expr` reads a `ScalarSubquery` anywhere in its scalar tree.
fn contains_scalar_subquery(expr: &ScalarExpr) -> bool {
    matches!(expr, ScalarExpr::ScalarSubquery(_))
        || expr.children().into_iter().any(contains_scalar_subquery)
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::ir::operator::operator_properties::WindowFuncKind;
    use asap_types::ir::operator::operator_properties::{
        ConcatDiscriminatorKey, GroupKeys, Source, WindowFrame, WindowFrameBound,
        WindowFrameOffset, WindowFrameUnits,
    };
    use asap_types::ir::schema::{DataType, Field, Schema};

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

    fn aggregate(
        reduction: Reduction,
        agg: AggIntent,
        child: Rc<OperatorNode>,
    ) -> Rc<OperatorNode> {
        node(NonASAPOp::Aggregate {
            reduction,
            measures: vec![agg],
            output_names: vec![],
            filters: vec![],
            having: None,
            child,
        })
    }

    fn count() -> AggIntent {
        AggIntent::Count {
            accuracy: AccuracyTarget::exact(),
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

    fn concat(
        children: Vec<Rc<OperatorNode>>,
        key: Option<ConcatDiscriminatorKey>,
    ) -> Rc<OperatorNode> {
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
        let Some(NonASAPOp::Aggregate {
            measures, child, ..
        }) = n.non_asap()
        else {
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

    // A heavy-hitter sketch ranks every row; a count that only counts some
    // rows (#466) is not that, so the generic Sort + Limit stays. Covers an
    // explicit `FILTER (WHERE latency > 1)` and the `IsNotNull` filter of
    // `COUNT(col)` over a nullable column.
    #[test]
    fn does_not_promote_a_filtered_count_ranking() {
        use asap_types::ir::scalar::{CompareOpKind, ScalarValue};
        use asap_types::ir::Predicate;
        let greater = Predicate(ScalarExpr::Compare {
            left: Box::new(ScalarExpr::Column(2)),
            op: CompareOpKind::Gt,
            right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(1.0))),
            semantics: asap_types::ir::ExprSemantics::Sql,
        });
        let not_null = Predicate(ScalarExpr::IsNotNull(Box::new(ScalarExpr::Column(2))));
        for filter in [greater, not_null] {
            let filtered = node(NonASAPOp::Aggregate {
                reduction: Reduction::by(vec![1]),
                measures: vec![count()],
                output_names: vec![],
                filters: vec![Some(filter)],
                having: None,
                child: scan(),
            });
            let q = limit(5, 0, sort(desc(1), filtered));
            let canonical = canonicalize(Rc::clone(&q)).unwrap();
            assert!(!is_topk_over_count(&canonical));
            assert!(Rc::ptr_eq(&canonical, &q));
        }
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

    /// `SELECT service FROM (SELECT service, COUNT(*) … ORDER BY 2 DESC
    /// LIMIT 5)`: the outer projection reads `service` by position, so the
    /// top-k under it stays `Limit{Sort}`; promoted, its column 0 would be
    /// the ranked value.
    #[test]
    fn does_not_promote_a_ranking_read_by_position() {
        let ranking = limit(5, 0, sort(desc(1), count_by_service()));
        let q = node(NonASAPOp::Project {
            cols: vec![ProjectItem {
                alias: None,
                expr: ScalarExpr::Column(0),
            }],
            qualifier: None,
            child: Rc::clone(&ranking),
        });
        let out = canonicalize(Rc::clone(&q)).unwrap();
        assert!(Rc::ptr_eq(&out, &q), "left as the same Project{{Limit}}");
    }

    #[test]
    fn rewritten_shared_subtree_stays_shared() {
        // One promotable sub-DAG referenced twice is rewritten once.
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
        assert!(
            out.schema.unique_keys.is_empty(),
            "the dropped key leaves the schema"
        );
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
    fn does_not_promote_a_global_limit_over_a_partitioned_sort() {
        // A global Limit keeps k rows overall, not k per Sort partition.
        let q = limit(
            5,
            0,
            node(NonASAPOp::Sort {
                keys: desc(1),
                partition_by: GroupKeys::by(vec![0]),
                child: count_by_service(),
            }),
        );
        assert!(!is_topk_over_count(&canonicalize(q).unwrap()));
    }

    #[test]
    fn promotes_a_limit_partitioned_like_its_sort() {
        let q = node(NonASAPOp::Limit {
            n: Some(5),
            offset: 0,
            partition_by: GroupKeys::by(vec![0]),
            child: node(NonASAPOp::Sort {
                keys: desc(1),
                partition_by: GroupKeys::by(vec![0]),
                child: count_by_service(),
            }),
        });
        assert!(is_topk_over_count(&canonicalize(q).unwrap()));
    }

    #[test]
    fn promotes_sum_ranked_limit_sort_as_weighted_heavy_hitter() {
        let sum = aggregate(Reduction::by(vec![1]), AggIntent::Sum { col: None }, scan());
        let out = canonicalize(limit(5, 0, sort(desc(1), sum))).unwrap();
        let Some(NonASAPOp::Aggregate {
            measures, child, ..
        }) = out.non_asap()
        else {
            panic!("expected weighted TopK aggregate");
        };
        assert!(matches!(
            measures.as_slice(),
            [AggIntent::TopK { k: 5, .. }]
        ));
        assert!(matches!(self::measures(child), [AggIntent::Sum { .. }]));
    }

    #[test]
    fn keeps_sum_over_counter_reduction_as_exact_value_ranking() {
        for counter in [AggIntent::Rate, AggIntent::Increase] {
            let derived = aggregate(Reduction::PerEntity, counter, scan());
            let sum = aggregate(
                Reduction::by(vec![1]),
                AggIntent::Sum { col: None },
                derived,
            );
            let out = canonicalize(limit(5, 0, sort(desc(1), sum))).unwrap();
            let Some(NonASAPOp::Limit { child, .. }) = out.non_asap() else {
                panic!("expected Limit, got {out:?}");
            };
            let Some(NonASAPOp::Sort { child, .. }) = child.non_asap() else {
                panic!("expected Sort under the Limit");
            };
            let Some(NonASAPOp::Aggregate {
                measures, child, ..
            }) = child.non_asap()
            else {
                panic!("expected Aggregate under the Sort");
            };
            assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
            assert!(matches!(
                child.non_asap(),
                Some(NonASAPOp::Aggregate { .. })
            ));
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

    /// `SQLWindowFunc{ RowNumber, PARTITION BY region(1), ORDER BY <order> }
    /// { agg }`, over `agg`'s `[service, region, <agg>]`; appends `rn` at 3.
    fn rownumber_window_by(order_by: Vec<SortKey>, agg: Rc<OperatorNode>) -> Rc<OperatorNode> {
        node(NonASAPOp::SQLWindowFunc {
            func: WindowFuncKind::RowNumber,
            args: vec![],
            partition_by: GroupKeys::by(vec![1]), // region
            order_by,
            frame: Some(rownumber_frame()),
            output_name: "rn".into(),
            child: agg,
        })
    }

    /// [`rownumber_window_by`] ordered by the aggregate output (col 2),
    /// descending, as `ORDER BY COUNT(*) DESC` lowers.
    fn rownumber_window(agg: Rc<OperatorNode>) -> Rc<OperatorNode> {
        rownumber_window_by(
            vec![SortKey {
                expr: ScalarExpr::Column(2),
                ascending: false,
                nulls_first: true,
            }],
            agg,
        )
    }

    /// `Filter{ col <op> k } { child }`.
    fn filter_cmp(
        col: usize,
        op: CompareOpKind,
        k: i64,
        child: Rc<OperatorNode>,
    ) -> Rc<OperatorNode> {
        node(NonASAPOp::Filter {
            pred: Predicate(ScalarExpr::Compare {
                left: Box::new(ScalarExpr::Column(col)),
                op,
                right: Box::new(ScalarExpr::Literal(ScalarValue::Int64(k))),
                semantics: ExprSemantics::Sql,
            }),
            child,
        })
    }

    /// `Filter{ col <= 5 } { child }`.
    fn filter_le_5(col: usize, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
        filter_cmp(col, CompareOpKind::Le, 5, child)
    }

    /// `Project{ cols }` naming each kept column as SQL does (`c` for the
    /// aggregate output).
    fn select(cols: &[usize], child: Rc<OperatorNode>) -> Rc<OperatorNode> {
        node(NonASAPOp::Project {
            cols: cols
                .iter()
                .map(|&col| ProjectItem {
                    alias: (col == 2).then(|| "c".to_string()),
                    expr: ScalarExpr::Column(col),
                })
                .collect(),
            qualifier: None,
            child,
        })
    }

    /// `SELECT service, region, c FROM (SELECT …, ROW_NUMBER() OVER
    /// (PARTITION BY region ORDER BY <agg> DESC) AS rn …) WHERE rn <= 5`:
    /// `Project{0,1,2} { Filter{ rn(3) <= 5 } { Project{0,1,2,3} { window
    /// { agg } } } }`, as the SQL frontend lowers it.
    fn rownumber_topk(agg: Rc<OperatorNode>) -> Rc<OperatorNode> {
        select(
            &[0, 1, 2],
            filter_le_5(3, select(&[0, 1, 2, 3], rownumber_window(agg))),
        )
    }

    fn assert_partitioned_sort_limit(out: &OperatorNode, k: usize, partition: usize) {
        let Some(NonASAPOp::Limit {
            n,
            offset: 0,
            partition_by,
            child,
        }) = out.non_asap()
        else {
            panic!("expected a Limit, got {out:?}");
        };
        assert_eq!(*n, Some(k));
        assert_eq!(*partition_by, vec![partition]);
        let Some(NonASAPOp::Sort { partition_by, .. }) = child.non_asap() else {
            panic!("expected a Sort under the Limit, got {child:?}");
        };
        assert_eq!(*partition_by, vec![partition]);
    }

    fn reaches_window(out: &Rc<OperatorNode>) -> bool {
        OperatorNode::reachable(out)
            .iter()
            .any(|n| matches!(n.non_asap(), Some(NonASAPOp::SQLWindowFunc { .. })))
    }

    /// A count-ranked `ROW_NUMBER` top-k is the heavy-hitter `TopK` PromQL
    /// `topk by (region)` lowers to: grouped by the window's partition
    /// (region, col 1 of the inner aggregate), over the inner Count.
    #[test]
    fn rownumber_count_topk_becomes_a_partitioned_heavy_hitter() {
        let agg = grouped(count());
        let out = canonicalize(rownumber_topk(Rc::clone(&agg))).unwrap();
        let Some(NonASAPOp::Aggregate {
            reduction,
            measures,
            child,
            ..
        }) = out.non_asap()
        else {
            panic!("expected an outer Aggregate([TopK]), got {out:?}");
        };
        assert!(matches!(
            measures.as_slice(),
            [AggIntent::TopK { k: 5, .. }]
        ));
        assert_eq!(*reduction.expect_reduce(), vec![1], "grouped by region");
        assert!(Rc::ptr_eq(child, &agg), "over the unchanged inner Count");
        assert_idempotent(&out);
    }

    /// `rn < 6` keeps the same rows as `rn <= 5`.
    #[test]
    fn rownumber_less_than_is_the_same_topk() {
        let lt = select(
            &[0, 1, 2],
            filter_cmp(
                3,
                CompareOpKind::Lt,
                6,
                select(&[0, 1, 2, 3], rownumber_window(grouped(count()))),
            ),
        );
        assert!(is_topk_over_count(&canonicalize(lt).unwrap()));
    }

    /// An avg ranking is no heavy-hitter: the generic per-partition
    /// `Limit{5}{Sort}`, with the query's columns unchanged.
    #[test]
    fn rownumber_avg_topk_becomes_a_partitioned_sort_limit() {
        let original = rownumber_topk(grouped(AggIntent::Avg { col: None }));
        let out = canonicalize(Rc::clone(&original)).unwrap();
        assert_eq!(out.schema, original.schema);
        assert_partitioned_sort_limit(&out, 5, 1);
        assert!(!reaches_window(&out));
        assert_idempotent(&out);
    }

    /// A tie-breaking second ORDER BY key is kept: the generic
    /// per-partition `Limit{Sort}` sorts by both keys.
    #[test]
    fn rownumber_with_a_tie_breaking_key_keeps_both_sort_keys() {
        let order = vec![
            SortKey {
                expr: ScalarExpr::Column(2),
                ascending: false,
                nulls_first: true,
            },
            SortKey {
                expr: ScalarExpr::Column(0),
                ascending: true,
                nulls_first: false,
            },
        ];
        let original = select(
            &[0, 1, 2],
            filter_le_5(
                3,
                select(
                    &[0, 1, 2, 3],
                    rownumber_window_by(order.clone(), grouped(count())),
                ),
            ),
        );
        let out = canonicalize(Rc::clone(&original)).unwrap();
        assert_eq!(out.schema, original.schema);
        assert_partitioned_sort_limit(&out, 5, 1);
        let Some(NonASAPOp::Limit { child: sort, .. }) = out.non_asap() else {
            unreachable!()
        };
        let Some(NonASAPOp::Sort { keys, .. }) = sort.non_asap() else {
            unreachable!()
        };
        assert_eq!(*keys, order);
    }

    /// A SELECT list without the partition column (`SELECT service, c`)
    /// still keeps 5 rows per region: the projection stays above the
    /// per-partition `Limit{Sort}` over the aggregate.
    #[test]
    fn rownumber_topk_selecting_fewer_columns_keeps_them_above_the_limit() {
        let original = select(
            &[0, 2],
            filter_le_5(3, select(&[0, 1, 2, 3], rownumber_window(grouped(count())))),
        );
        let out = canonicalize(Rc::clone(&original)).unwrap();
        assert_eq!(out.schema, original.schema);
        let Some(NonASAPOp::Project { child, .. }) = out.non_asap() else {
            panic!("expected the SELECT list on top, got {out:?}");
        };
        assert_partitioned_sort_limit(child, 5, 1);
        assert!(!reaches_window(&out));
        assert_idempotent(&out);
    }

    /// `rn = 1` (the top row per region) is `rn <= 1`; `rn <= 0`, `rn < 1`
    /// and `rn = 2` are no top-k and keep the window.
    #[test]
    fn rownumber_bounds() {
        let with = |op, k| {
            select(
                &[0, 1, 2],
                filter_cmp(
                    3,
                    op,
                    k,
                    select(&[0, 1, 2, 3], rownumber_window(grouped(count()))),
                ),
            )
        };
        let top_1 = canonicalize(with(CompareOpKind::Eq, 1)).unwrap();
        assert!(matches!(measures(&top_1), [AggIntent::TopK { k: 1, .. }]));
        for (op, k) in [
            (CompareOpKind::Le, 0),
            (CompareOpKind::Lt, 1),
            (CompareOpKind::Eq, 2),
        ] {
            let q = with(op, k);
            assert!(Rc::ptr_eq(&canonicalize(Rc::clone(&q)).unwrap(), &q), "{k}");
        }
    }

    /// Under a consumer reading its columns by position (`SELECT service
    /// FROM (<per-region top 5>)`), the ROW_NUMBER top-k becomes the
    /// per-partition `Limit{Sort}` with the same columns, not promoted.
    #[test]
    fn rownumber_topk_read_by_position_is_not_promoted() {
        let original = select(&[0], rownumber_topk(grouped(count())));
        let out = canonicalize(Rc::clone(&original)).unwrap();
        assert_eq!(out.schema, original.schema);
        let Some(NonASAPOp::Project { child, .. }) = out.non_asap() else {
            panic!("the outer SELECT stays, got {out:?}");
        };
        assert_partitioned_sort_limit(child, 5, 1);
        assert_idempotent(&out);
    }

    /// A `Concat` read by position passes that on to its branches: a
    /// promotable branch under `Project{Concat}` stays `Limit{Sort}`.
    #[test]
    fn concat_branches_read_through_a_positional_reader_are_not_promoted() {
        let branch = limit(5, 0, sort(desc(1), count_by_service()));
        let q = passthrough_project(concat(vec![Rc::clone(&branch), count_by_service()], None));
        let out = canonicalize(Rc::clone(&q)).unwrap();
        assert!(Rc::ptr_eq(&out, &q));
    }

    /// Without an enclosing SELECT dropping it, `rn` is an output column:
    /// the window stays.
    #[test]
    fn a_visible_row_number_is_left_alone() {
        let q = filter_le_5(3, select(&[0, 1, 2, 3], rownumber_window(grouped(count()))));
        let out = canonicalize(Rc::clone(&q)).unwrap();
        assert!(Rc::ptr_eq(&out, &q), "left as the same Filter");
    }

    #[test]
    fn filter_on_a_non_rownumber_column_is_left_alone() {
        // `WHERE service_len <= 5` (col 0, not the rn window column) must not
        // be mistaken for a top-k.
        let q = select(
            &[0, 1, 2],
            filter_le_5(0, select(&[0, 1, 2, 3], rownumber_window(grouped(count())))),
        );
        let out = canonicalize(Rc::clone(&q)).unwrap();
        assert!(Rc::ptr_eq(&out, &q), "left as the same Project");
    }

    // ── Subquery lowering ───────────────────────────────────────────────────

    fn filter_of(pred: ScalarExpr, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
        node(NonASAPOp::Filter {
            pred: Predicate(pred),
            child,
        })
    }

    /// `SELECT service FROM scan` — a one-column subquery.
    /// Scalar reads retain cardinality/null semantics and a shared producer.
    #[test]
    fn scalar_subqueries_remain_explicit_and_shared() {
        let sub = one_column_subquery();
        let root = node(NonASAPOp::Project {
            child: scan(),
            qualifier: None,
            cols: vec![
                ProjectItem {
                    alias: Some("a".into()),
                    expr: ScalarExpr::ScalarSubquery(Rc::clone(&sub)),
                },
                ProjectItem {
                    alias: Some("b".into()),
                    expr: ScalarExpr::ScalarSubquery(Rc::clone(&sub)),
                },
            ],
        });
        let out = canonicalize(root).unwrap();
        let NonASAPOp::Project { cols, child, .. } = out.expect_non_asap() else {
            panic!()
        };
        assert!(matches!(child.expect_non_asap(), NonASAPOp::Scan { .. }));
        for col in cols {
            assert!(matches!(&col.expr,ScalarExpr::ScalarSubquery(node) if Rc::ptr_eq(node,&sub)));
        }
        assert!(out.schema.fields.iter().all(|f| f.nullable));
        assert_idempotent(&out);
    }

    fn one_column_subquery() -> Rc<OperatorNode> {
        node(NonASAPOp::Scan {
            source: Source::Table {
                table_ref: "sub".into(),
            },
            predicates: vec![],
            schema: Schema::new(vec![Field::plain("value", DataType::Float64, false)]),
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
        // The subquery is a count-ranked `Limit{Sort}`. The join reads its
        // columns, so it is not promoted (that would change them); once
        // lifted into the join it is canonical, so a second pass finds
        // nothing to do.
        let sub = limit(5, 0, sort(desc(1), count_by_service()));
        let q = filter_of(exists(Rc::clone(&sub), false), scan());
        let out = canonicalize(q).unwrap();
        let (_, _, _, r) = join_parts(&out);
        assert!(Rc::ptr_eq(r, &sub));
        assert_idempotent(&out);
    }
}
