//! [`SemanticEquivalentRewriteStrategy`] — algebraic, semantic-equivalent
//! query rewriting,
//! the third bullet in `docs/design_docs/asap_aware_mapping.md`'s "Degrees of freedom"
//! section: "Semantic-equivalent rewriting (e.g. `avg` → `sum`/`count`) to
//! increase how often the [sharing/sketch] optimizations above apply"
//! (issue #253, part of #33, per Peilin's #33 comment).
//!
//! ## Why `avg` needs this and `sum`/`count` don't
//!
//! [`replacement::realizations_for_intent`] dispatches `AggIntent::Avg`
//! straight to `Realization::PassThrough` — see that module's own
//! comment on why: `Avg`/`StdDev`/`Variance` "need richer partial state"
//! than a bare sketch/exact accumulator gives, so there is no summary
//! realization for a bare `avg` node to bind to at all. A logical `avg`
//! node therefore can never be a [`SharedSubDagStrategy`] target either:
//! CSE-style sharing needs *some* mergeable accumulator underneath, and
//! `PassThrough` has none.
//!
//! `Sum` and `Count` are both ordinary mergeable accumulators
//! (`agg_is_mergeable`) — exactly the shape [`SharedSubDagStrategy`] and a
//! future sketch-family search already know how to reuse across a
//! workload. Rewriting `Aggregate{ measures: [Avg{col}], .. }` into two
//! independent single-measure `Sum` and `Count` aggregates, divided with a
//! `BinaryOp`, computes the same result but *reshapes* it into targets other
//! strategies can bind and share independently. This module only performs
//! that reshaping — see "Non-goals" below for why it does not also decide
//! whether the reshaping is worth it.
//!
//! ## Scope
//!
//! Ordinary `by(...)` averages use a schema-preserving projection. Temporal
//! Float64 temporal averages require a typed finite-division guard; their
//! sum/count components are never exported as an unconditional logical rewrite.
//!
//! - **`without(...)` grouping** leaves an `Aggregate`'s own output schema
//!   *open* (`closed: false`, see `without_output_schema`), while the
//!   `Project` this strategy always wraps the rewrite in forces
//!   `closed: true` (see `NonASAPOp::output_schema`'s `Project` arm). Under
//!   `without(...)` the rewritten form's `closed` flag would silently flip
//!   relative to the original — exactly the kind of schema drift this
//!   module exists to avoid.
//!
//! Both are follow-ups (issue #253 itself scopes to "the concrete case in
//! Peilin's comment"), not correctness bugs in what ships here — a node
//! outside this scope simply doesn't `match`, the same "safe but
//! uninformative" fallback [`ASAPStrategies`]/[`SharedSubDagStrategy`]
//! already use for shapes they don't have an opinion on.
//!
//! ## Non-goals (mirrors [`replacement`]'s own discipline)
//!
//! **No "is it worth it" heuristic.** An earlier draft of this idea needed a
//! manual before/after-CSE cost comparison to decide whether rewriting
//! helps. With `ReplacementStrategy`'s exhaustive-candidate shape in place,
//! that's unnecessary: this strategy just reports the rewritten form as one
//! more [`ReplacementSubDAG`] alongside whatever else applies to the same
//! target. A future cost-based search (issue #252) is what decides whether
//! the rewritten form is actually worth picking, by letting the original
//! and rewritten forms compete on cost — not this strategy.

use asap_types::ir::non_asap::any_measure_filtered;
use std::rc::Rc;

use asap_types::ir::operator_properties::{BinaryOpKind, Reduction};
use asap_types::ir::{BinaryOperator, NonASAPOp, OperatorNode, ProjectItem, ScalarExpr};
use asap_types::pre_asap::agg_intent::AggIntent;
use asap_types::pre_asap::expr_ir::ArithmeticOpKind;
use asap_types::pre_asap::schema::{ColumnId, DataType};

use asap_types::types::AccuracyTarget;

use crate::replacement::{Replacement, ReplacementStrategy, ReplacementSubDAG, TargetSubDAG};

/// The shape [`AvgToSumOverCountStrategy`] rewrites: a single `Avg{col}`
/// measure, no `HAVING`, grouped with an ordinary `by(...)` reduction (see
/// the module docs' "Scope" for why `without(...)`/`PerEntity` are
/// excluded). Returns the grouping key count and the summed column so
/// [`build_rewrite`] doesn't have to re-match.
/// `a / b` with PromQL arithmetic semantics and no vector matching.
fn arithmetic(
    op: ArithmeticOpKind,
    lhs: Rc<OperatorNode>,
    rhs: Rc<OperatorNode>,
) -> Option<Rc<OperatorNode>> {
    OperatorNode::non_asap_node(NonASAPOp::BinaryOp {
        operator: BinaryOperator {
            checked_relative_division: false,
            checked_finite_division: false,
            kind: BinaryOpKind::Arithmetic(op),
            vector_match: None,
        },
        return_bool: false,
        lhs,
        rhs,
    })
    .ok()
}

fn avg_rewrite_target(node: &OperatorNode) -> Option<(usize, Option<ColumnId>)> {
    let Some(NonASAPOp::Aggregate {
        reduction,
        measures,
        filters,
        having: None,
        child,
        ..
    }) = node.non_asap()
    else {
        return None;
    };
    if any_measure_filtered(filters) {
        return None;
    }
    let Reduction::Reduce(by) = reduction else {
        return None;
    };
    if by.is_without() {
        return None;
    }
    let [AggIntent::Avg { col }] = measures.as_slice() else {
        return None;
    };
    // `AggIntent::Count` represents COUNT(*), not COUNT(col).  AVG(col) can
    // therefore be decomposed through it only when the averaged input is
    // provably non-null; otherwise NULL rows would incorrectly contribute to
    // the denominator.
    let input_schema = &child.schema;
    let value_col = col
        .or_else(|| input_schema.column_id("value"))
        .or_else(|| (0..input_schema.fields.len()).find(|i| !by.contains(i)))?;
    if input_schema.fields.get(value_col)?.nullable {
        return None;
    }
    Some((by.keys().len(), *col))
}

/// Build the rewritten `Project{ cast(sum) } / Aggregate{ Count }` tree for
/// `root`, or `None` if `root` isn't [`avg_rewrite_target`]'s shape. `Sum` and
/// `Count` deliberately live in separate, single-measure aggregates so the
/// replacement fixpoint discovers each as an independently bindable target.
///
/// The `Project`'s leading `by.len()` items are bare `Column(i)`
/// pass-throughs of the grouping keys — identical in name/type to the
/// original `Avg` aggregate's own leading columns, since both aggregates
/// share the same `reduction`/`child` and only differ in `measures`
/// (`aggregate_output_schema`'s grouping-column derivation never looks at
/// `measures` at all). The final item recomputes `sum / count`, casting the
/// numerator to `Float64` before division, and aliases it to the original
/// `avg` column's own name — matching
/// [`AggIntent::Avg::output_column`]'s `(name, Float64, nullable: false)`
/// exactly regardless of the summed column's own type (integer division
/// would otherwise silently reappear whenever the input column is itself
/// integer-typed: `Sum`'s output type tracks its input, `Count`'s is always
/// `Int64`, and `ScalarExpr::scalar_type`'s own `Arithmetic` type inference
/// types a `Div` of two `Int64` operands as `Int64` — the explicit operand
/// `Cast` is what keeps both the division and rewritten `avg` column
/// `Float64` the way the original always was, not an incidental extra step).
// These are conditional physical components, never an unconditional Rewrite.
// The caller must attach the finite-division execution guard before admission.
pub(crate) fn temporal_average_components(root: &Rc<OperatorNode>) -> Option<Rc<OperatorNode>> {
    let Some(NonASAPOp::Aggregate {
        reduction: Reduction::PerEntity,
        measures,
        filters,
        child,
        having: None,
        ..
    }) = root.non_asap()
    else {
        return None;
    };
    if any_measure_filtered(filters) {
        return None;
    }
    let [AggIntent::Avg { col }] = measures.as_slice() else {
        return None;
    };
    if !matches!(child.non_asap(), Some(NonASAPOp::TimeRange { .. })) {
        return None;
    }
    let schema = &child.schema;
    let value = schema
        .fields
        .get(col.or_else(|| schema.column_id("value"))?)?;
    if value.nullable || value.dtype != DataType::Float64 {
        return None;
    }
    let aggregate = |intent| {
        OperatorNode::non_asap_node(NonASAPOp::Aggregate {
            reduction: Reduction::PerEntity,
            measures: vec![intent],
            output_names: vec![],
            filters: vec![],
            having: None,
            child: Rc::clone(child),
        })
        .ok()
    };
    let rewritten = arithmetic(
        ArithmeticOpKind::Div,
        aggregate(AggIntent::Sum { col: *col })?,
        aggregate(AggIntent::Count {
            accuracy: AccuracyTarget::Exact,
        })?,
    )?;
    (root.schema == rewritten.schema).then_some(rewritten)
}

fn build_rewrite(root: &Rc<OperatorNode>) -> Option<Rc<OperatorNode>> {
    let (group_count, col) = avg_rewrite_target(root)?;
    let Some(NonASAPOp::Aggregate {
        reduction,
        output_names,
        child,
        ..
    }) = root.non_asap()
    else {
        unreachable!("avg_rewrite_target already confirmed an Aggregate shape");
    };

    // The original `avg` column's own name: `output_names[0]` if the
    // producing front end overrode it (SQL threading DataFusion's own
    // generated name — see `NonASAPOp::Aggregate::output_names`'s docs),
    // else `AggIntent::Avg`'s synthetic default. Either way this is the
    // *only* thing about the original output column this rewrite needs to
    // reproduce — `AggIntent::Avg::output_column`'s `(Float64, nullable:
    // false)` half is already reproduced structurally (see `build_rewrite`'s
    // own doc comment) rather than looked up here.
    let avg_name = output_names
        .first()
        .filter(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| "avg".to_string());

    let sum_agg = OperatorNode::non_asap_node(NonASAPOp::Aggregate {
        reduction: reduction.clone(),
        measures: vec![AggIntent::Sum { col }],
        output_names: Vec::new(),
        filters: vec![],
        having: None,
        child: Rc::clone(child),
    })
    .ok()?;
    let count_agg = OperatorNode::non_asap_node(NonASAPOp::Aggregate {
        reduction: reduction.clone(),
        measures: vec![AggIntent::Count {
            accuracy: AccuracyTarget::Exact,
        }],
        output_names: Vec::new(),
        filters: vec![],
        having: None,
        child: Rc::clone(child),
    })
    .ok()?;

    let sum_idx = group_count;
    let mut cols: Vec<ProjectItem> = (0..group_count)
        .map(|i| ProjectItem {
            alias: None,
            expr: ScalarExpr::Column(i),
        })
        .collect();
    cols.push(ProjectItem {
        alias: Some(avg_name),
        expr: ScalarExpr::Cast {
            expr: Box::new(ScalarExpr::Column(sum_idx)),
            to: DataType::Float64,
            try_cast: false,
        },
    });

    let float_sum = OperatorNode::non_asap_node(NonASAPOp::Project {
        cols,
        qualifier: None,
        child: sum_agg,
    })
    .ok()?;
    arithmetic(ArithmeticOpKind::Div, float_sum, count_agg)
}

/// Compose adjacent per-entity and cross-entity accumulators when their
/// algebra, rather than a query-language spelling, proves equivalence.
pub(crate) fn composed_aggregate_rewrite(root: &Rc<OperatorNode>) -> Option<Rc<OperatorNode>> {
    let original_schema = &root.schema;
    let Some(NonASAPOp::Aggregate {
        reduction: outer_reduction @ Reduction::Reduce(_),
        measures: outer_measures,
        output_names,
        filters: outer_filters,
        having: None,
        child,
    }) = root.non_asap()
    else {
        return None;
    };
    let Some(NonASAPOp::Aggregate {
        reduction: Reduction::PerEntity,
        measures: inner_measures,
        filters: inner_filters,
        having: None,
        child: inner_child,
        ..
    }) = child.non_asap()
    else {
        return None;
    };
    if any_measure_filtered(outer_filters) || any_measure_filtered(inner_filters) {
        return None;
    }
    let ([outer], [inner]) = (outer_measures.as_slice(), inner_measures.as_slice()) else {
        return None;
    };
    let composed = match (outer, inner) {
        (AggIntent::Sum { col: None }, AggIntent::Sum { .. })
        | (AggIntent::Sum { col: None }, AggIntent::Count { .. })
        | (AggIntent::Min { col: None }, AggIntent::Min { .. })
        | (AggIntent::Max { col: None }, AggIntent::Max { .. }) => inner.clone(),
        _ => return None,
    };
    let aggregate = OperatorNode::non_asap_node(NonASAPOp::Aggregate {
        reduction: outer_reduction.clone(),
        measures: vec![composed],
        output_names: output_names.clone(),
        filters: vec![],
        having: None,
        child: Rc::clone(inner_child),
    })
    .ok()?;

    // The outer Sum sees PromQL's Float64 sample value, whereas the composed
    // Count accumulator is Int64. Keep the original observable type.
    let rewritten = if matches!(
        (outer, inner),
        (AggIntent::Sum { col: None }, AggIntent::Count { .. })
    ) {
        let Reduction::Reduce(by) = outer_reduction else {
            unreachable!()
        };
        if by.is_without() {
            return None;
        }
        let mut cols: Vec<ProjectItem> = (0..by.keys().len())
            .map(|i| ProjectItem {
                alias: None,
                expr: ScalarExpr::Column(i),
            })
            .collect();
        cols.push(ProjectItem {
            // PromQL deliberately supplies an empty output-name override. Use
            // the already-derived caller-visible name instead of allowing
            // Project to invent `col_N` and then failing schema equality.
            alias: original_schema
                .fields
                .last()
                .map(|column| column.name.clone()),
            expr: ScalarExpr::Cast {
                expr: Box::new(ScalarExpr::Column(by.keys().len())),
                to: DataType::Float64,
                try_cast: false,
            },
        });
        OperatorNode::non_asap_node(NonASAPOp::Project {
            cols,
            qualifier: None,
            child: aggregate,
        })
        .ok()?
    } else {
        aggregate
    };

    // Positional keys, aliases, types, and nullability are part of the rule's
    // contract. A future schema change therefore disables rather than widens
    // the rewrite.
    (*original_schema == rewritten.schema).then_some(rewritten)
}

/// Rewrites `Aggregate{ measures: [Avg{col}], .. }` into the semantically
/// equivalent pair of single-measure `Sum` and `Count` aggregates divided by
/// a `BinaryOp` — see the module docs for why keeping the accumulators in
/// separate relational nodes lets later strategies reach them independently.
///
/// A unit struct: unlike [`ASAPStrategies`], this strategy doesn't
/// bind anything (its one [`Replacement`] is always [`Replacement::Rewrite`],
/// never [`Replacement::Summary`]) and so has no [`CostModel`](crate::CostModel)
/// to hold a reference to — the same "no state needed" shape
/// [`SharedSubDagStrategy`] already has.
#[derive(Debug, Default, Clone, Copy)]
pub struct SemanticEquivalentRewriteStrategy;

/// Backward-compatible name for callers that registered the original, narrower
/// average rewrite. It now denotes the same semantic-rewrite strategy.
pub use SemanticEquivalentRewriteStrategy as AvgToSumOverCountStrategy;

impl ReplacementStrategy for SemanticEquivalentRewriteStrategy {
    fn matches(&self, target: &TargetSubDAG<'_>) -> bool {
        avg_rewrite_target(target.root).is_some()
            || composed_aggregate_rewrite(target.root).is_some()
    }

    fn replacements(&self, target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG> {
        if let Some(rewritten) = composed_aggregate_rewrite(target.root) {
            return vec![ReplacementSubDAG {
                strategy: "SemanticEquivalentRewriteStrategy",
                replacement: Replacement::SubDag(rewritten),
                provenance: crate::replacement::ReplacementProvenance::LogicalRewrite,
                rationale: "compose compatible per-entity and cross-entity accumulators using their algebraic intent while preserving the original output schema".into(),
            }];
        }
        let Some(rewritten) = build_rewrite(target.root) else {
            return Vec::new();
        };
        vec![ReplacementSubDAG {
            strategy: "AvgToSumOverCountStrategy",
            replacement: Replacement::SubDag(rewritten),
            provenance: crate::replacement::ReplacementProvenance::LogicalRewrite,
            rationale:
                "avg has no summary realization at all (replacement::realizations_for_intent \
                        dispatches it to PassThrough) and so can never share or sketch; \
                        rewriting it into sum/count under the same grouping — re-divided back \
                        into the original avg column by a wrapping Project — computes the same \
                        result from two ordinary mergeable accumulators SharedSubDagStrategy \
                        (and a future sketch-family search) can actually reuse across the \
                        workload"
                    .to_string(),
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::lower_promql;
    use asap_types::ir::operator_properties::Source;
    use asap_types::pre_asap::schema::{Field, Schema};
    use asap_types::types::AccuracyTarget;
    use std::time::Duration;

    use crate::test_support::metric_scan;
    use asap_types::ir::TimeRangeKind;

    fn avg_agg(
        by: Vec<ColumnId>,
        col: Option<ColumnId>,
        child: Rc<OperatorNode>,
    ) -> Rc<OperatorNode> {
        avg_agg_with(by, col, vec![], None, child)
    }

    fn avg_agg_with(
        by: Vec<ColumnId>,
        col: Option<ColumnId>,
        output_names: Vec<String>,
        having: Option<asap_types::ir::Predicate>,
        child: Rc<OperatorNode>,
    ) -> Rc<OperatorNode> {
        OperatorNode::non_asap_node(NonASAPOp::Aggregate {
            reduction: Reduction::by(by),
            measures: vec![AggIntent::Avg { col }],
            output_names,
            filters: vec![],
            having,
            child,
        })
        .unwrap()
    }

    // Temporal averages expose two single-measure children without closing labels.
    #[test]
    fn temporal_average_components_preserves_schema_and_exposes_sum_count() {
        let root = lower_promql("avg_over_time(a{job=\"api\"}[5m])", AccuracyTarget::Exact);
        assert!(SemanticEquivalentRewriteStrategy
            .replacements(&TargetSubDAG::new(&root))
            .is_empty());
        let rewritten =
            temporal_average_components(&root).expect("conditional sum/count components");
        assert_eq!(root.schema.clone(), rewritten.schema.clone());
        assert!(matches!(
            rewritten.non_asap(),
            Some(NonASAPOp::BinaryOp { .. })
        ));
    }

    // ── matches ──────────────────────────────────────────────────────────

    #[test]
    fn matches_a_bare_avg_aggregate() {
        let q = avg_agg(vec![], None, metric_scan(&[]));
        let target = TargetSubDAG::new(&q);
        assert!(AvgToSumOverCountStrategy.matches(&target));
    }

    #[test]
    fn matches_a_grouped_avg_aggregate() {
        let q = avg_agg(vec![2], None, metric_scan(&["job"]));
        let target = TargetSubDAG::new(&q);
        assert!(AvgToSumOverCountStrategy.matches(&target));
    }

    #[test]
    fn does_not_match_a_multi_measure_aggregate() {
        let q = OperatorNode::non_asap_node(NonASAPOp::Aggregate {
            reduction: Reduction::by(vec![2]),
            measures: vec![AggIntent::Sum { col: None }, AggIntent::Avg { col: None }],
            output_names: vec![],
            filters: vec![],
            having: None,
            child: metric_scan(&["job"]),
        })
        .unwrap();
        let target = TargetSubDAG::new(&q);
        assert!(!AvgToSumOverCountStrategy.matches(&target));
        assert!(AvgToSumOverCountStrategy.replacements(&target).is_empty());
    }

    #[test]
    fn does_not_match_a_having_bearing_avg_aggregate() {
        let q = avg_agg_with(
            vec![2],
            None,
            vec![],
            Some(asap_types::ir::Predicate(ScalarExpr::Literal(
                asap_types::pre_asap::expr_ir::ScalarValue::Boolean(true),
            ))),
            metric_scan(&["job"]),
        );
        let target = TargetSubDAG::new(&q);
        assert!(!AvgToSumOverCountStrategy.matches(&target));
        assert!(AvgToSumOverCountStrategy.replacements(&target).is_empty());
    }

    #[test]
    fn does_not_match_a_non_avg_intent() {
        for intent in [
            AggIntent::Sum { col: None },
            AggIntent::Count {
                accuracy: AccuracyTarget::Exact,
            },
            AggIntent::Min { col: None },
        ] {
            let q = OperatorNode::non_asap_node(NonASAPOp::Aggregate {
                reduction: Reduction::by(vec![2]),
                measures: vec![intent.clone()],
                output_names: vec![],
                filters: vec![],
                having: None,
                child: metric_scan(&["job"]),
            })
            .unwrap();
            let target = TargetSubDAG::new(&q);
            assert!(
                !AvgToSumOverCountStrategy.matches(&target),
                "expected no match for {intent:?}"
            );
            assert!(AvgToSumOverCountStrategy.replacements(&target).is_empty());
        }
    }

    #[test]
    fn does_not_match_a_without_grouped_avg_aggregate() {
        let q = OperatorNode::non_asap_node(NonASAPOp::Aggregate {
            reduction: Reduction::Reduce(asap_types::ir::operator_properties::GroupKeys::without(
                vec![2],
            )),
            measures: vec![AggIntent::Avg { col: None }],
            output_names: vec![],
            filters: vec![],
            having: None,
            child: metric_scan(&["job"]),
        })
        .unwrap();
        let target = TargetSubDAG::new(&q);
        assert!(!AvgToSumOverCountStrategy.matches(&target));
        assert!(AvgToSumOverCountStrategy.replacements(&target).is_empty());
    }

    #[test]
    fn does_not_match_a_per_entity_avg_aggregate() {
        let q = OperatorNode::non_asap_node(NonASAPOp::Aggregate {
            reduction: Reduction::PerEntity,
            measures: vec![AggIntent::Avg { col: None }],
            output_names: vec![],
            filters: vec![],
            having: None,
            child: metric_scan(&[]),
        })
        .unwrap();
        let target = TargetSubDAG::new(&q);
        assert!(!AvgToSumOverCountStrategy.matches(&target));
        assert!(AvgToSumOverCountStrategy.replacements(&target).is_empty());
    }

    #[test]
    fn does_not_match_a_non_aggregate_node() {
        let scan = metric_scan(&["job"]);
        let target = TargetSubDAG::new(&scan);
        assert!(!AvgToSumOverCountStrategy.matches(&target));
        assert!(AvgToSumOverCountStrategy.replacements(&target).is_empty());
    }

    // ── replacements / schema round-trip ─────────────────────────────────

    /// The rewritten tree must keep `Sum` and `Count` in separate aggregates,
    /// and its `output_schema()` must equal the original `Avg` aggregate's.
    #[test]
    fn avg_rewrites_and_schema_matches_exactly_when_ungrouped() {
        let original = avg_agg(vec![], None, metric_scan(&[]));
        let original_rc = Rc::clone(&original);
        let target = TargetSubDAG::new(&original_rc);

        let replacements = AvgToSumOverCountStrategy.replacements(&target);
        assert_eq!(replacements.len(), 1, "{replacements:?}");
        assert!(!replacements[0].rationale.is_empty());

        let rewritten = match &replacements[0].replacement {
            Replacement::SubDag(rc) => rc,
            other => panic!("expected a Rewrite replacement, got {other:?}"),
        };
        let Some(NonASAPOp::BinaryOp { lhs, rhs, .. }) = rewritten.non_asap() else {
            panic!("expected sum/count BinaryOp, got {rewritten:?}");
        };
        let Some(NonASAPOp::Project { child: sum, .. }) = lhs.non_asap() else {
            panic!("expected cast Project above Sum, got {lhs:?}");
        };
        assert!(matches!(sum.non_asap(),
            Some(NonASAPOp::Aggregate { measures, .. })
                if matches!(measures.as_slice(), [AggIntent::Sum { col: None }])
        ));
        assert!(matches!(rhs.non_asap(),
            Some(NonASAPOp::Aggregate { measures, .. })
                if matches!(measures.as_slice(), [AggIntent::Count { accuracy: AccuracyTarget::Exact }])
        ));

        let original_schema = original.schema.clone();
        let rewritten_schema = rewritten.schema.clone();
        assert_eq!(
            original_schema, rewritten_schema,
            "the rewritten tree must report exactly the same output schema as the original avg"
        );
    }

    /// A named (SQL-style) output override survives the rewrite: the
    /// `Project`'s final column is re-aliased to `output_names[0]`, not the
    /// synthetic `"avg"` default.
    #[test]
    fn preserves_an_explicit_output_name_override() {
        let q = avg_agg_with(
            vec![],
            None,
            vec!["avg_latency".to_string()],
            None,
            metric_scan(&[]),
        );
        let original_schema = q.schema.clone();
        let target = TargetSubDAG::new(&q);

        let replacements = AvgToSumOverCountStrategy.replacements(&target);
        let rewritten = match &replacements[0].replacement {
            Replacement::SubDag(rc) => rc,
            other => panic!("expected a Rewrite replacement, got {other:?}"),
        };
        let rewritten_schema = rewritten.schema.clone();
        assert_eq!(original_schema, rewritten_schema);
        assert_eq!(rewritten_schema.fields[0].name, "avg_latency");
    }

    /// A grouped rewrite must preserve the aggregate's grouping-key metadata;
    /// CSE and roll-up legality both depend on it.
    #[test]
    fn grouped_avg_rewrite_preserves_the_whole_schema() {
        let original = avg_agg(vec![2], None, metric_scan(&["job"]));
        let original_schema = original.schema.clone();
        let original_rc = Rc::clone(&original);
        let target = TargetSubDAG::new(&original_rc);

        let replacements = AvgToSumOverCountStrategy.replacements(&target);
        let rewritten = match &replacements[0].replacement {
            Replacement::SubDag(rc) => rc,
            other => panic!("expected a Rewrite replacement, got {other:?}"),
        };
        let rewritten_schema = rewritten.schema.clone();

        assert_eq!(rewritten_schema, original_schema);
    }

    #[test]
    fn default_search_discovers_bindable_sum_and_count_targets() {
        let root = avg_agg(vec![2], None, metric_scan(&["job"]));
        let space = crate::replacement::search_workload(vec![("avg", Rc::clone(&root))]);

        let avg_group = space
            .candidates_for_target(&space.roots[0].1)
            .expect("avg group");
        assert!(avg_group.candidates.iter().any(|candidate| {
            candidate.provenance == crate::replacement::ReplacementProvenance::LogicalRewrite
        }));

        let mut found_sum = false;
        let mut found_count = false;
        for group in space.target_subdag_candidates() {
            let Some(NonASAPOp::Aggregate { measures, .. }) = group.target.non_asap() else {
                continue;
            };
            let expected = matches!(measures.as_slice(), [AggIntent::Sum { .. }])
                || matches!(
                    measures.as_slice(),
                    [AggIntent::Count {
                        accuracy: AccuracyTarget::Exact
                    }]
                );
            if !expected {
                continue;
            }
            assert!(
                group
                    .candidates
                    .iter()
                    .any(|candidate| matches!(&candidate.replacement,
                        Replacement::SubDag(node) if node.contains_asap())),
                "rewritten accumulator must be independently bindable: {measures:?}"
            );
            found_sum |= matches!(measures.as_slice(), [AggIntent::Sum { .. }]);
            found_count |= matches!(
                measures.as_slice(),
                [AggIntent::Count {
                    accuracy: AccuracyTarget::Exact
                }]
            );
        }
        assert!(found_sum && found_count);
    }

    #[test]
    fn works_with_a_bound_column_not_just_the_sample_value() {
        let mut schema_cols = vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("job", DataType::Utf8, true),
            Field::plain("bytes", DataType::Int64, false),
        ];
        let child = OperatorNode::non_asap_node(NonASAPOp::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: {
                let cols = std::mem::take(&mut schema_cols);
                Schema::with_time_index(cols, 0, vec![])
            },
        })
        .unwrap();
        let original = avg_agg(vec![1], Some(2), child);
        let original_schema = original.schema.clone();
        let original_rc = Rc::clone(&original);
        let target = TargetSubDAG::new(&original_rc);

        let replacements = AvgToSumOverCountStrategy.replacements(&target);
        let rewritten = match &replacements[0].replacement {
            Replacement::SubDag(rc) => rc,
            other => panic!("expected a Rewrite replacement, got {other:?}"),
        };
        let rewritten_schema = rewritten.schema.clone();

        // The whole reason for the explicit `Cast` in `build_rewrite`: an
        // `Int64` input column (`bytes`) makes `Sum`'s own output `Int64`
        // too, and a bare (uncast) `Int64 / Int64` would type the avg
        // column `Int64` — this assertion is what would catch that
        // regression.
        assert_eq!(rewritten_schema.fields, original_schema.fields);
        assert_eq!(
            rewritten_schema.fields.last().unwrap().dtype,
            DataType::Float64
        );

        let Some(NonASAPOp::BinaryOp { lhs, .. }) = rewritten.non_asap() else {
            panic!("expected sum/count BinaryOp");
        };
        let Some(NonASAPOp::Project { cols, .. }) = lhs.non_asap() else {
            panic!("expected cast Project above Sum");
        };
        assert!(matches!(
            &cols.last().unwrap().expr,
            ScalarExpr::Cast {
                to: DataType::Float64,
                ..
            }
        ));
    }

    #[test]
    fn does_not_rewrite_avg_of_a_nullable_column_via_count_star() {
        let child = OperatorNode::non_asap_node(NonASAPOp::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Field::plain("ts", DataType::Timestamp, false),
                    Field::plain("value", DataType::Float64, false),
                    Field::plain("latency", DataType::Float64, true),
                ],
                0,
                vec![],
            ),
        })
        .unwrap();
        let q = avg_agg(vec![], Some(2), child);
        let target = TargetSubDAG::new(&q);

        assert!(!AvgToSumOverCountStrategy.matches(&target));
        assert!(AvgToSumOverCountStrategy.replacements(&target).is_empty());
    }

    fn nested_aggregate(outer: AggIntent, inner: AggIntent) -> Rc<OperatorNode> {
        let temporal = OperatorNode::non_asap_node(NonASAPOp::Aggregate {
            reduction: Reduction::PerEntity,
            measures: vec![inner],
            output_names: vec![],
            filters: vec![],
            having: None,
            child: OperatorNode::non_asap_node(NonASAPOp::TimeRange {
                kind: TimeRangeKind::Range,
                range: Duration::from_secs(300),
                child: metric_scan(&["service"]),
            })
            .unwrap(),
        })
        .unwrap();
        OperatorNode::non_asap_node(NonASAPOp::Aggregate {
            reduction: Reduction::by(vec![2]),
            measures: vec![outer],
            // Match the PromQL front end: an empty entry selects the intent's
            // canonical output name rather than an explicit alias.
            output_names: vec![String::new()],
            filters: vec![],
            having: None,
            child: temporal,
        })
        .unwrap()
    }

    #[test]
    fn semantic_rewrite_composes_every_supported_aggregate_pair() {
        let exact_count = AggIntent::Count {
            accuracy: AccuracyTarget::Exact,
        };
        for (outer, inner) in [
            (AggIntent::Sum { col: None }, AggIntent::Sum { col: None }),
            (AggIntent::Sum { col: None }, exact_count),
            (AggIntent::Min { col: None }, AggIntent::Min { col: None }),
            (AggIntent::Max { col: None }, AggIntent::Max { col: None }),
        ] {
            let expected = inner.clone();
            let original = nested_aggregate(outer, inner);
            let candidates =
                SemanticEquivalentRewriteStrategy.replacements(&TargetSubDAG::new(&original));
            let [candidate] = candidates.as_slice() else {
                panic!("supported pair should produce exactly one rewrite")
            };
            let Replacement::SubDag(rewritten) = &candidate.replacement else {
                panic!("expected a logical rewrite")
            };
            assert_eq!(original.schema.clone(), rewritten.schema.clone());
            let aggregate = match rewritten.non_asap() {
                Some(NonASAPOp::Aggregate { .. }) => rewritten.as_ref(),
                Some(NonASAPOp::Project { child, .. }) => child.as_ref(),
                other => panic!("expected Aggregate or cast Project, got {other:?}"),
            };
            let Some(NonASAPOp::Aggregate {
                reduction: Reduction::Reduce(by),
                measures,
                child,
                ..
            }) = aggregate.non_asap()
            else {
                panic!("expected composed cross-entity aggregate")
            };
            assert_eq!(by.keys(), &[2]);
            assert_eq!(measures, &[expected]);
            assert!(matches!(child.non_asap(),
                Some(NonASAPOp::TimeRange { range, child, .. })
                    if *range == Duration::from_secs(300)
                        && matches!(child.non_asap(), Some(NonASAPOp::Scan { .. }))
            ));
        }
    }

    #[test]
    fn semantic_rewrite_rejects_non_composable_aggregate_pairs() {
        for (outer, inner) in [
            (AggIntent::Sum { col: None }, AggIntent::Min { col: None }),
            (AggIntent::Avg { col: None }, AggIntent::Sum { col: None }),
            (AggIntent::Min { col: None }, AggIntent::Max { col: None }),
        ] {
            let original = nested_aggregate(outer, inner);
            assert!(composed_aggregate_rewrite(&original).is_none());
        }
    }

    #[test]
    fn default_search_discovers_promql_shaped_sum_count_composition() {
        let root = nested_aggregate(
            AggIntent::Sum { col: None },
            AggIntent::Count {
                accuracy: AccuracyTarget::Exact,
            },
        );
        let space = crate::replacement::search_workload(vec![("sum-count", root)]);
        let root = &space.roots[0].1;
        let group = space.candidates_for_target(root).expect("root memo group");
        let candidate = group
            .candidates
            .iter()
            .find(|candidate| candidate.strategy == "SemanticEquivalentRewriteStrategy")
            .expect("default search should run semantic rewrites");
        let Replacement::SubDag(rewritten) = &candidate.replacement else {
            panic!("expected logical rewrite")
        };
        assert_eq!(rewritten.schema.clone().fields[1].name, "sum");
    }
}
