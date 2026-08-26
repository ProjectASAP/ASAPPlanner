//! [`AvgToSumOverCountStrategy`] — semantic-equivalent query rewriting,
//! the third bullet in `docs/design_docs/asap_aware_mapping.md`'s "Degrees of freedom"
//! section: "Semantic-equivalent rewriting (e.g. `avg` → `sum`/`count`) to
//! increase how often the [sharing/sketch] optimizations above apply"
//! (issue #253, part of #33, per Peilin's #33 comment).
//!
//! ## Why `avg` needs this and `sum`/`count` don't
//!
//! [`replacement::implementations_for_with`] dispatches `AggIntent::Avg`
//! straight to `Implementation::PassThrough` — see that module's own
//! comment on why: `Avg`/`StdDev`/`Variance` "need richer partial state"
//! than a bare sketch/exact accumulator gives, so there is no summary
//! realization for a bare `avg` node to bind to at all. A logical `avg`
//! node therefore can never be a [`SharedSubtreeStrategy`] target either:
//! CSE-style sharing needs *some* mergeable accumulator underneath, and
//! `PassThrough` has none.
//!
//! `Sum` and `Count` are both ordinary mergeable accumulators
//! (`agg_is_mergeable`) — exactly the shape [`SharedSubtreeStrategy`] and a
//! future sketch-family search already know how to reuse across a
//! workload. Rewriting `Aggregate{ measures: [Avg{col}], .. }` into two
//! independent single-measure `Sum` and `Count` aggregates, divided with a
//! `BinaryOp`, computes the same result but *reshapes* it into targets other
//! strategies can bind and share independently. This module only performs
//! that reshaping — see "Non-goals" below for why it does not also decide
//! whether the reshaping is worth it.
//!
//! ## Scope: `by(...)` grouping only (issue #253's own scope note)
//!
//! [`AvgToSumOverCountStrategy::matches`] additionally requires
//! `Reduction::Reduce(by)` with `by` an ordinary (non-`without`) grouping —
//! narrower than [`SketchAlgorithmStrategy`]'s `bindable_intent`, which is
//! `Reduction`-agnostic. Two concrete reasons, not stylistic ones:
//!
//! - **`Reduction::PerEntity`** (`rate`/`increase`/`*_over_time`) is
//!   single-measure by construction —
//!   [`aggregate_output_schema`](asap_types::pre_asap::query_expr::aggregate_output_schema)
//!   `debug_assert!`s exactly one measure for it. This rewrite's entire
//!   point is introducing a *second* measure (`Count` alongside `Sum`)
//!   under the same node, which would violate that invariant outright, not
//!   just drift a schema detail.
//! - **`without(...)` grouping** leaves an `Aggregate`'s own output schema
//!   *open* (`closed: false`, see `without_output_schema`), while the
//!   `Project` this strategy always wraps the rewrite in forces
//!   `closed: true` (see `QueryExpr::output_schema`'s `Project` arm). Under
//!   `without(...)` the rewritten form's `closed` flag would silently flip
//!   relative to the original — exactly the kind of schema drift this
//!   module exists to avoid.
//!
//! Both are follow-ups (issue #253 itself scopes to "the concrete case in
//! Peilin's comment"), not correctness bugs in what ships here — a node
//! outside this scope simply doesn't `match`, the same "safe but
//! uninformative" fallback [`SketchAlgorithmStrategy`]/[`SharedSubtreeStrategy`]
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

use std::rc::Rc;

use asap_types::pre_asap::agg_intent::AggIntent;
use asap_types::pre_asap::expr_ir::ArithmeticOpKind;
use asap_types::pre_asap::query_expr::{BinaryOpKind, ProjectItem, QueryExpr, Reduction};
use asap_types::pre_asap::schema::{ColumnId, DataType};
use asap_types::types::AccuracyTarget;

use crate::replacement::{Replacement, ReplacementStrategy, ReplacementSubDAG, TargetSubDAG};

/// The shape [`AvgToSumOverCountStrategy`] rewrites: a single `Avg{col}`
/// measure, no `HAVING`, grouped with an ordinary `by(...)` reduction (see
/// the module docs' "Scope" for why `without(...)`/`PerEntity` are
/// excluded). Returns the grouping key count and the summed column so
/// [`build_rewrite`] doesn't have to re-match.
fn avg_rewrite_target(node: &QueryExpr) -> Option<(usize, Option<ColumnId>)> {
    let QueryExpr::Aggregate {
        reduction,
        measures,
        having: None,
        child,
        ..
    } = node
    else {
        return None;
    };
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
    let input_schema = child.output_schema().ok()?;
    let value_col = col
        .or_else(|| input_schema.column_id("value"))
        .or_else(|| (0..input_schema.columns.len()).find(|i| !by.contains(i)))?;
    if input_schema.columns.get(value_col)?.nullable {
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
/// `Int64`, and `QueryExpr::output_schema`'s own `Arithmetic` type inference
/// types a `Div` of two `Int64` operands as `Int64` — the explicit operand
/// `Cast` is what keeps both the division and rewritten `avg` column
/// `Float64` the way the original always was, not an incidental extra step).
fn build_rewrite(root: &Rc<QueryExpr>) -> Option<Rc<QueryExpr>> {
    let (group_count, col) = avg_rewrite_target(root)?;
    let QueryExpr::Aggregate {
        reduction,
        output_names,
        child,
        ..
    } = root.as_ref()
    else {
        unreachable!("avg_rewrite_target already confirmed an Aggregate shape");
    };

    // The original `avg` column's own name: `output_names[0]` if the
    // producing front end overrode it (SQL threading DataFusion's own
    // generated name — see `QueryExpr::Aggregate::output_names`'s docs),
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

    let sum_agg = Rc::new(QueryExpr::Aggregate {
        reduction: reduction.clone(),
        measures: vec![AggIntent::Sum { col }],
        output_names: Vec::new(),
        having: None,
        child: Rc::clone(child),
    });
    let count_agg = Rc::new(QueryExpr::Aggregate {
        reduction: reduction.clone(),
        measures: vec![AggIntent::Count {
            accuracy: AccuracyTarget::Exact,
        }],
        output_names: Vec::new(),
        having: None,
        child: Rc::clone(child),
    });

    let sum_idx = group_count;
    let mut cols: Vec<ProjectItem> = (0..group_count)
        .map(|i| ProjectItem {
            alias: None,
            expr: QueryExpr::Column(i),
        })
        .collect();
    cols.push(ProjectItem {
        alias: Some(avg_name),
        expr: QueryExpr::Cast {
            expr: Rc::new(QueryExpr::Column(sum_idx)),
            to: DataType::Float64,
            try_cast: false,
        },
    });

    let float_sum = Rc::new(QueryExpr::Project {
        cols,
        qualifier: None,
        child: sum_agg,
    });
    Some(Rc::new(QueryExpr::BinaryOp {
        op: BinaryOpKind::Arithmetic(ArithmeticOpKind::Div),
        lhs: float_sum,
        rhs: count_agg,
        vector_match: None,
    }))
}

/// Rewrites `Aggregate{ measures: [Avg{col}], .. }` into the semantically
/// equivalent pair of single-measure `Sum` and `Count` aggregates divided by
/// a `BinaryOp` — see the module docs for why keeping the accumulators in
/// separate relational nodes lets later strategies reach them independently.
///
/// A unit struct: unlike [`SketchAlgorithmStrategy`], this strategy doesn't
/// bind anything (its one [`Replacement`] is always [`Replacement::Rewrite`],
/// never [`Replacement::Summary`]) and so has no [`CostModel`](crate::CostModel)
/// to hold a reference to — the same "no state needed" shape
/// [`SharedSubtreeStrategy`] already has.
#[derive(Debug, Default, Clone, Copy)]
pub struct AvgToSumOverCountStrategy;

impl ReplacementStrategy for AvgToSumOverCountStrategy {
    fn matches(&self, target: &TargetSubDAG<'_>) -> bool {
        avg_rewrite_target(target.root).is_some()
    }

    fn replacements(&self, target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG> {
        let Some(rewritten) = build_rewrite(target.root) else {
            return Vec::new();
        };
        vec![ReplacementSubDAG {
            strategy: "AvgToSumOverCountStrategy",
            replacement: Replacement::Rewrite(rewritten),
            provenance: crate::replacement::ReplacementProvenance::LogicalRewrite,
            rationale:
                "avg has no summary realization at all (replacement::implementations_for_with \
                        dispatches it to PassThrough) and so can never share or sketch; \
                        rewriting it into sum/count under the same grouping — re-divided back \
                        into the original avg column by a wrapping Project — computes the same \
                        result from two ordinary mergeable accumulators SharedSubtreeStrategy \
                        (and a future sketch-family search) can actually reuse across the \
                        workload"
                    .to_string(),
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::pre_asap::query_expr::Source;
    use asap_types::pre_asap::schema::{Column, Schema};

    fn metric_scan(labels: &[&str]) -> QueryExpr {
        let mut columns = vec![
            Column::new("ts", DataType::Timestamp, false),
            Column::new("value", DataType::Float64, false),
        ];
        columns.extend(labels.iter().map(|n| Column::new(*n, DataType::Utf8, true)));
        QueryExpr::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(columns, 0, vec![]),
        }
    }

    fn avg_agg(by: Vec<ColumnId>, col: Option<ColumnId>, child: QueryExpr) -> QueryExpr {
        QueryExpr::Aggregate {
            reduction: Reduction::by(by),
            measures: vec![AggIntent::Avg { col }],
            output_names: vec![],
            having: None,
            child: Rc::new(child),
        }
    }

    // ── matches ──────────────────────────────────────────────────────────

    #[test]
    fn matches_a_bare_avg_aggregate() {
        let q = Rc::new(avg_agg(vec![], None, metric_scan(&[])));
        let target = TargetSubDAG::new(&q);
        assert!(AvgToSumOverCountStrategy.matches(&target));
    }

    #[test]
    fn matches_a_grouped_avg_aggregate() {
        let q = Rc::new(avg_agg(vec![2], None, metric_scan(&["job"])));
        let target = TargetSubDAG::new(&q);
        assert!(AvgToSumOverCountStrategy.matches(&target));
    }

    #[test]
    fn does_not_match_a_multi_measure_aggregate() {
        let q = Rc::new(QueryExpr::Aggregate {
            reduction: Reduction::by(vec![2]),
            measures: vec![AggIntent::Sum { col: None }, AggIntent::Avg { col: None }],
            output_names: vec![],
            having: None,
            child: Rc::new(metric_scan(&["job"])),
        });
        let target = TargetSubDAG::new(&q);
        assert!(!AvgToSumOverCountStrategy.matches(&target));
        assert!(AvgToSumOverCountStrategy.replacements(&target).is_empty());
    }

    #[test]
    fn does_not_match_a_having_bearing_avg_aggregate() {
        let mut q = avg_agg(vec![2], None, metric_scan(&["job"]));
        if let QueryExpr::Aggregate { having, .. } = &mut q {
            *having = Some(asap_types::pre_asap::query_expr::Predicate(Rc::new(
                QueryExpr::Literal(asap_types::pre_asap::expr_ir::ScalarValue::Boolean(true)),
            )));
        }
        let q = Rc::new(q);
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
            let q = Rc::new(QueryExpr::Aggregate {
                reduction: Reduction::by(vec![2]),
                measures: vec![intent.clone()],
                output_names: vec![],
                having: None,
                child: Rc::new(metric_scan(&["job"])),
            });
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
        let q = Rc::new(QueryExpr::Aggregate {
            reduction: Reduction::Reduce(asap_types::pre_asap::query_expr::GroupKeys::without(
                vec![2],
            )),
            measures: vec![AggIntent::Avg { col: None }],
            output_names: vec![],
            having: None,
            child: Rc::new(metric_scan(&["job"])),
        });
        let target = TargetSubDAG::new(&q);
        assert!(!AvgToSumOverCountStrategy.matches(&target));
        assert!(AvgToSumOverCountStrategy.replacements(&target).is_empty());
    }

    #[test]
    fn does_not_match_a_per_entity_avg_aggregate() {
        let q = Rc::new(QueryExpr::Aggregate {
            reduction: Reduction::PerEntity,
            measures: vec![AggIntent::Avg { col: None }],
            output_names: vec![],
            having: None,
            child: Rc::new(metric_scan(&[])),
        });
        let target = TargetSubDAG::new(&q);
        assert!(!AvgToSumOverCountStrategy.matches(&target));
        assert!(AvgToSumOverCountStrategy.replacements(&target).is_empty());
    }

    #[test]
    fn does_not_match_a_non_aggregate_node() {
        let scan = Rc::new(metric_scan(&["job"]));
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
        let original_rc = Rc::new(original.clone());
        let target = TargetSubDAG::new(&original_rc);

        let replacements = AvgToSumOverCountStrategy.replacements(&target);
        assert_eq!(replacements.len(), 1, "{replacements:?}");
        assert!(!replacements[0].rationale.is_empty());

        let rewritten = match &replacements[0].replacement {
            Replacement::Rewrite(rc) => rc,
            other => panic!("expected a Rewrite replacement, got {other:?}"),
        };
        let QueryExpr::BinaryOp { lhs, rhs, .. } = rewritten.as_ref() else {
            panic!("expected sum/count BinaryOp, got {rewritten:?}");
        };
        let QueryExpr::Project { child: sum, .. } = lhs.as_ref() else {
            panic!("expected cast Project above Sum, got {lhs:?}");
        };
        assert!(matches!(
            sum.as_ref(),
            QueryExpr::Aggregate { measures, .. }
                if matches!(measures.as_slice(), [AggIntent::Sum { col: None }])
        ));
        assert!(matches!(
            rhs.as_ref(),
            QueryExpr::Aggregate { measures, .. }
                if matches!(measures.as_slice(), [AggIntent::Count { accuracy: AccuracyTarget::Exact }])
        ));

        let original_schema = original.output_schema().unwrap();
        let rewritten_schema = rewritten.output_schema().unwrap();
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
        let mut q = avg_agg(vec![], None, metric_scan(&[]));
        if let QueryExpr::Aggregate { output_names, .. } = &mut q {
            *output_names = vec!["avg_latency".to_string()];
        }
        let original_schema = q.output_schema().unwrap();
        let q = Rc::new(q);
        let target = TargetSubDAG::new(&q);

        let replacements = AvgToSumOverCountStrategy.replacements(&target);
        let rewritten = match &replacements[0].replacement {
            Replacement::Rewrite(rc) => rc,
            other => panic!("expected a Rewrite replacement, got {other:?}"),
        };
        let rewritten_schema = rewritten.output_schema().unwrap();
        assert_eq!(original_schema, rewritten_schema);
        assert_eq!(rewritten_schema.columns[0].name, "avg_latency");
    }

    /// A grouped rewrite must preserve the aggregate's grouping-key metadata;
    /// CSE and roll-up legality both depend on it.
    #[test]
    fn grouped_avg_rewrite_preserves_the_whole_schema() {
        let original = avg_agg(vec![2], None, metric_scan(&["job"]));
        let original_schema = original.output_schema().unwrap();
        let original_rc = Rc::new(original);
        let target = TargetSubDAG::new(&original_rc);

        let replacements = AvgToSumOverCountStrategy.replacements(&target);
        let rewritten = match &replacements[0].replacement {
            Replacement::Rewrite(rc) => rc,
            other => panic!("expected a Rewrite replacement, got {other:?}"),
        };
        let rewritten_schema = rewritten.output_schema().unwrap();

        assert_eq!(rewritten_schema, original_schema);
    }

    #[test]
    fn default_search_discovers_bindable_sum_and_count_targets() {
        let root = Rc::new(avg_agg(vec![2], None, metric_scan(&["job"])));
        let space = crate::replacement::search_workload(vec![("avg", Rc::clone(&root))]);

        let avg_group = space.group_for(&space.roots[0].1).expect("avg group");
        assert!(avg_group.candidates.iter().any(|candidate| {
            candidate.provenance == crate::replacement::ReplacementProvenance::LogicalRewrite
        }));

        let mut found_sum = false;
        let mut found_count = false;
        for group in space.groups() {
            let QueryExpr::Aggregate { measures, .. } = group.target.as_ref() else {
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
                    .any(|candidate| matches!(candidate.replacement, Replacement::Summary(_))),
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
            Column::new("ts", DataType::Timestamp, false),
            Column::new("job", DataType::Utf8, true),
            Column::new("bytes", DataType::Int64, false),
        ];
        let child = QueryExpr::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: {
                let cols = std::mem::take(&mut schema_cols);
                Schema::with_time_index(cols, 0, vec![])
            },
        };
        let original = avg_agg(vec![1], Some(2), child);
        let original_schema = original.output_schema().unwrap();
        let original_rc = Rc::new(original);
        let target = TargetSubDAG::new(&original_rc);

        let replacements = AvgToSumOverCountStrategy.replacements(&target);
        let rewritten = match &replacements[0].replacement {
            Replacement::Rewrite(rc) => rc,
            other => panic!("expected a Rewrite replacement, got {other:?}"),
        };
        let rewritten_schema = rewritten.output_schema().unwrap();

        // The whole reason for the explicit `Cast` in `build_rewrite`: an
        // `Int64` input column (`bytes`) makes `Sum`'s own output `Int64`
        // too, and a bare (uncast) `Int64 / Int64` would type the avg
        // column `Int64` — this assertion is what would catch that
        // regression.
        assert_eq!(rewritten_schema.columns, original_schema.columns);
        assert_eq!(
            rewritten_schema.columns.last().unwrap().dtype,
            DataType::Float64
        );

        let QueryExpr::BinaryOp { lhs, .. } = rewritten.as_ref() else {
            panic!("expected sum/count BinaryOp");
        };
        let QueryExpr::Project { cols, .. } = lhs.as_ref() else {
            panic!("expected cast Project above Sum");
        };
        assert!(matches!(
            &cols.last().unwrap().expr,
            QueryExpr::Cast {
                to: DataType::Float64,
                ..
            }
        ));
    }

    #[test]
    fn does_not_rewrite_avg_of_a_nullable_column_via_count_star() {
        let child = QueryExpr::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Column::new("ts", DataType::Timestamp, false),
                    Column::new("value", DataType::Float64, false),
                    Column::new("latency", DataType::Float64, true),
                ],
                0,
                vec![],
            ),
        };
        let q = Rc::new(avg_agg(vec![], Some(2), child));
        let target = TargetSubDAG::new(&q);

        assert!(!AvgToSumOverCountStrategy.matches(&target));
        assert!(AvgToSumOverCountStrategy.replacements(&target).is_empty());
    }
}
