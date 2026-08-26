//! [`AccuracyReconciliationStrategy`] — cross-consumer accuracy
//! reconciliation for CSE sharing (issue #273, part of #33).
//!
//! ## The gap this closes
//!
//! `asap_types::pre_asap::cse::share_common_subtrees` (pre-ASAP CSE) only
//! ever merges two subtrees that are *exactly* [`PartialEq`]-equal,
//! including their [`AggIntent`]'s `accuracy: AccuracyTarget` field. Two
//! otherwise-identical aggregates that differ *only* in how tight an
//! accuracy bound they ask for — `quantile(0.99, x)` at `epsilon=0.01` for
//! one consumer, the same `quantile(0.99, x)` at `epsilon=0.05` for
//! another — are therefore never the same `Rc`, never collapse into one
//! [`crate::replacement::MemoGroup`], and [`crate::replacement::SharedSubtreeStrategy`]
//! never even gets a `TargetSubDAG` with `consumer_count >= 2` to propose
//! sharing for. This crate would build two entirely independent sketches
//! for what is conceptually one computation, even though a single sketch
//! built to the tighter of the two bounds would answer both.
//!
//! This module is **additive**, not a relaxation of `share_common_subtrees`
//! itself: `accuracy` still participates in exact structural equality
//! everywhere else in this crate (correctness elsewhere — e.g. a downstream
//! consumer that pattern-matches on a specific `AccuracyTarget` — depends on
//! that). What this module adds is a *second*, narrower notion of "close
//! enough to share" that sits entirely inside the [`ReplacementStrategy`]
//! extension point: one more candidate a [`crate::cost_model::CostModel`]
//! may or may not prefer, never a forced rewrite and never a change to what
//! `share_common_subtrees` itself merges.
//!
//! ## What counts as a "near-duplicate", and why
//!
//! Two [`QueryExpr::Aggregate`] nodes are accuracy-near-duplicates here iff,
//! **in this order**:
//!
//! 1. Both are the same bindable shape [`crate::replacement::SketchAlgorithmStrategy`]
//!    itself targets — a single measure, no `HAVING` (`bindable_intent`'s own
//!    scope) — **and** that one measure is one of the four accuracy-bearing
//!    [`AggIntent`] variants ([`crate::replacement::accuracy_target`]'s own
//!    scope: `Count` / `Quantile` / `Cardinality` / `TopK`). Every other
//!    intent has no `AccuracyTarget` to reconcile in the first place.
//! 2. Same `reduction` (grouping), same `output_names`, and the same shared
//!    `child` (`Rc::ptr_eq`, or value-equal for two independently-built but
//!    identical subtrees CSE conservatively declined to alias) — the same
//!    "identical everything else" bar [`crate::rollup::RollupStrategy`] and
//!    [`crate::topk_reuse::TopKLimitReuseStrategy`] already hold their own
//!    sibling-reuse candidates to.
//! 3. The one measure is identical **except** for `accuracy` — same variant,
//!    same `col`/`q`/`k` (see [`same_intent_except_accuracy`]).
//! 4. Neither side's `accuracy` is [`AccuracyTarget::Exact`] (see
//!    [`dominates`]'s doc for why exact accuracy is excluded rather than
//!    trivially "always tightest").
//!
//! ## Safety of tightening: why reading the tighter build is always sound
//!
//! [`crate::replacement::accuracy_budget`] resolves *every* `AccuracyTarget`
//! (`Epsilon`/`EpsilonDelta`) to the literal `(eps, delta)` pair
//! `implementations_for_with`'s `sketch_implementations` feeds into
//! `CostModel::size_params` — the same numbers `default_size_params`'
//! `kll_k` / `cms_width` / `cms_depth` / `hll_precision` / `kmv_k` / DDSketch's
//! own `alpha == eps` invert. Every one of those formulas is monotonic in its
//! input: a smaller `eps` (or `delta`) never produces a smaller/less-accurate
//! structure. So a sketch sized to budget `(e1, d1)` also satisfies any
//! requirement `(e2, d2)` with `e1 <= e2 && d1 <= d2` — [`dominates`]'s exact
//! check — regardless of which of `Epsilon`/`EpsilonDelta` either side is
//! spelled as, because both resolve through the identical `accuracy_budget`
//! mapping before sizing ever happens. Combined with "same [`AggIntent`]
//! variant apart from accuracy" (point 3 above), both consumers rank/select
//! from the exact same [`crate::replacement::summary_candidates`] family
//! list, so tightening never changes *which* sketch algorithm would be
//! picked — only how tightly its parameters are sized. That is the whole
//! safety argument this module leans on: **never** a claim that some
//! `AccuracyTarget` is "close enough" by fuzzy/heuristic similarity, always
//! a literal Pareto-domination check on the exact numbers a build would be
//! sized with.
//!
//! `AccuracyTarget::Exact` is deliberately excluded from both sides (see
//! [`dominates`]): `implementations_for_with` routes it to `exact_realization`
//! instead of `sketch_implementations` — a different `Implementation` family
//! entirely, not just a tighter budget within the same one — so "build once
//! at the tighter of the two" doesn't mean the same thing there. Reconciling
//! an exact consumer with an approximate one is a different, larger question
//! (does an exact accumulator ever make sense to share with a sketch
//! consumer, cost-wise?) this module leaves alone rather than answers
//! speculatively.
//!
//! Cross-shape comparison (`Epsilon` vs. `EpsilonDelta`) is *not* a design
//! question left open here the way the issue's own "Why this wasn't done in
//! #259" section worried about — `accuracy_budget` already commits both
//! shapes to concrete `(eps, delta)` numbers today (an `Epsilon(e)` resolves
//! to `(e, DEFAULT_DELTA)`), so [`dominates`] compares those resolved numbers
//! directly rather than inventing a second, shape-aware ordering.
//!
//! ## Never forced
//!
//! Like every [`ReplacementStrategy`], this only ever *proposes* — the
//! looser-accuracy consumer's own independently-sized candidate (from
//! [`crate::replacement::SketchAlgorithmStrategy`]) stays in its
//! [`crate::replacement::MemoGroup`] right alongside this strategy's
//! "read the tighter sibling instead" [`Replacement::Rewrite`] candidate;
//! [`crate::cost_model::CostModel`]-driven ranking (`rank_group`'s own
//! "candidate may be handled by more than one strategy" fallback —
//! `cost_model.estimate_cost`, the same path an existing mixed-strategy group
//! like [`crate::topk_reuse::TopKLimitReuseStrategy`]'s already exercises)
//! picks between them; nothing here removes or filters the independent
//! candidate.

use std::cmp::Ordering;
use std::rc::Rc;

use asap_types::pre_asap::agg_intent::AggIntent;
use asap_types::pre_asap::query_expr::{QueryExpr, Reduction};
use asap_types::types::AccuracyTarget;

use crate::replacement::{
    accuracy_budget, accuracy_target, Replacement, ReplacementProvenance, ReplacementStrategy,
    ReplacementSubDAG, TargetSubDAG,
};

/// `bindable_accuracy_aggregate`'s return shape: `(reduction, intent,
/// accuracy, output_names, child)` — factored into its own alias per
/// `clippy::type_complexity`, not a semantic distinction.
type BindableAccuracyAggregate<'a> = (
    &'a Reduction,
    &'a AggIntent,
    &'a AccuracyTarget,
    &'a [String],
    &'a Rc<QueryExpr>,
);

/// The `(reduction, intent, accuracy, output_names, child)` shape this
/// module operates on: the same single-measure, no-`HAVING` bindable shape
/// [`crate::replacement::SketchAlgorithmStrategy`] targets (see that
/// module's private `bindable_intent`), further narrowed to a measure whose
/// intent actually carries an [`AccuracyTarget`]
/// ([`crate::replacement::accuracy_target`]'s own scope: `Count` /
/// `Quantile` / `Cardinality` / `TopK`). `None` for anything else, including
/// a multi-measure or `HAVING` aggregate, a non-`Aggregate` node, or an
/// accuracy-free intent (`Sum`, `Avg`, …).
fn bindable_accuracy_aggregate(node: &QueryExpr) -> Option<BindableAccuracyAggregate<'_>> {
    let QueryExpr::Aggregate {
        reduction,
        measures,
        output_names,
        having,
        child,
    } = node
    else {
        return None;
    };
    let ([intent], None) = (measures.as_slice(), having) else {
        return None;
    };
    let accuracy = accuracy_target(intent)?;
    Some((reduction, intent, accuracy, output_names.as_slice(), child))
}

/// Are `a` and `b` the identical [`AggIntent`] apart from `accuracy` —
/// same variant, same `col`/`q`/`k`? Only ever called with two
/// accuracy-bearing intents (both `bindable_accuracy_aggregate`-gated
/// first), but written as a full match rather than assuming that, the same
/// defensive-completeness style [`crate::replacement::describe_intent`]
/// uses for its own non-exhaustive `AggIntent` match.
fn same_intent_except_accuracy(a: &AggIntent, b: &AggIntent) -> bool {
    match (a, b) {
        (AggIntent::Count { .. }, AggIntent::Count { .. }) => true,
        (
            AggIntent::Quantile { col: c1, q: q1, .. },
            AggIntent::Quantile { col: c2, q: q2, .. },
        ) => c1 == c2 && q1 == q2,
        (AggIntent::TopK { k: k1, .. }, AggIntent::TopK { k: k2, .. }) => k1 == k2,
        (AggIntent::Cardinality { col: c1, .. }, AggIntent::Cardinality { col: c2, .. }) => {
            c1 == c2
        }
        _ => false,
    }
}

/// Would a build sized to `tighter`'s accuracy requirement also satisfy
/// `looser`'s? See the module docs' "Safety of tightening" section for the
/// full argument; this is the Pareto check that argument reduces to: both
/// sides resolve through [`accuracy_budget`] to concrete `(eps, delta)`
/// numbers, and `tighter` dominates `looser` iff neither of its two numbers
/// is larger.
///
/// `AccuracyTarget::Exact` on either side always returns `false` — never a
/// dominator, never dominated. Numerically, `accuracy_budget(Exact)`
/// resolves to a budget that would Pareto-dominate everything (zero error),
/// but `implementations_for_with` realizes `Exact` through a wholly
/// different code path (`exact_realization`, never `sketch_implementations`)
/// — a different `Implementation` family, not a point on the same sizing
/// curve — so the numeric comparison alone does not mean what it means for
/// two approximate targets. See the module docs for the full reasoning.
fn dominates(tighter: &AccuracyTarget, looser: &AccuracyTarget) -> bool {
    if matches!(tighter, AccuracyTarget::Exact) || matches!(looser, AccuracyTarget::Exact) {
        return false;
    }
    let (tighter_eps, tighter_delta) = accuracy_budget(tighter);
    let (looser_eps, looser_delta) = accuracy_budget(looser);
    tighter_eps <= looser_eps && tighter_delta <= looser_delta
}

/// `dominates(a, b)` and the two resolved budgets aren't equal — the
/// **strict** ordering [`AccuracyReconciliationStrategy`] actually needs.
/// Without strictness, two `AccuracyTarget`s that resolve to the identical
/// `(eps, delta)` budget via different spellings (e.g. `Epsilon(0.01)` vs.
/// `EpsilonDelta { epsilon: 0.01, delta: DEFAULT_DELTA }`) would each
/// `dominates` the other, and this module would propose "read the sibling
/// instead" both ways for zero actual benefit — a degenerate tie, not a real
/// choice. Requiring strictness means only a build that is genuinely tighter
/// somewhere ever gets proposed as a replacement for a looser one.
fn strictly_tighter(a: &AccuracyTarget, b: &AccuracyTarget) -> bool {
    dominates(a, b) && accuracy_budget(a) != accuracy_budget(b)
}

/// Reconciles near-duplicate [`AggIntent`]s that differ only in their
/// [`AccuracyTarget`] (issue #273, part of #33): given a looser-accuracy
/// [`TargetSubDAG`], finds every already-known sibling `Aggregate` that
/// computes the identical intent over the identical input at a strictly
/// tighter accuracy, and proposes reading that sibling's own (to-be-built)
/// result instead of building an independent, looser copy — "build once at
/// the tightest of the group's accuracy requirements, all consumers read
/// from it," ranked by [`crate::cost_model::CostModel`] like any other
/// candidate, never forced. See the module docs for the full design.
///
/// `siblings` is **caller-supplied, not discovered here** — the identical
/// "workload-wide discovery isn't this strategy's job" split
/// [`crate::rollup::RollupStrategy`] and [`crate::topk_reuse::TopKLimitReuseStrategy`]
/// already draw; [`crate::replacement::search_workload_with`] constructs
/// this strategy from the same post-CSE `Aggregate` sibling set it already
/// builds for `RollupStrategy`.
pub struct AccuracyReconciliationStrategy {
    siblings: Vec<Rc<QueryExpr>>,
}

impl AccuracyReconciliationStrategy {
    /// A strategy that owns clones of every node in `siblings` and considers
    /// each as a candidate tighter-accuracy source (or looser-accuracy
    /// target) — typically the full set of `Aggregate` nodes a workload-wide
    /// discovery pass already found.
    pub fn new(siblings: &[Rc<QueryExpr>]) -> Self {
        Self {
            siblings: siblings.to_vec(),
        }
    }

    /// Every sibling that is a legal, strictly-tighter accuracy source for
    /// `target` — shared between `matches` and `replacements` so the two can
    /// never disagree about which siblings qualify. Sorted tightest-last
    /// (arbitrary but deterministic order over the exhaustive candidate
    /// list — this strategy makes no claim about which tighter source a
    /// `CostModel` should prefer).
    fn tighter_sources<'a>(&'a self, target: &TargetSubDAG<'_>) -> Vec<&'a Rc<QueryExpr>> {
        let Some((target_reduction, target_intent, target_accuracy, target_names, target_child)) =
            bindable_accuracy_aggregate(target.root)
        else {
            return Vec::new();
        };

        let mut sources: Vec<&Rc<QueryExpr>> = self
            .siblings
            .iter()
            .filter(|candidate| {
                if Rc::ptr_eq(candidate, target.root) {
                    return false;
                }
                let Some((reduction, intent, accuracy, names, child)) =
                    bindable_accuracy_aggregate(candidate)
                else {
                    return false;
                };
                reduction == target_reduction
                    && names == target_names
                    && (Rc::ptr_eq(child, target_child) || child == target_child)
                    && same_intent_except_accuracy(intent, target_intent)
                    && strictly_tighter(accuracy, target_accuracy)
            })
            .collect();
        sources.sort_by(|a, b| {
            let (.., a_accuracy, _, _) =
                bindable_accuracy_aggregate(a).expect("filter above already confirmed this shape");
            let (.., b_accuracy, _, _) =
                bindable_accuracy_aggregate(b).expect("filter above already confirmed this shape");
            accuracy_budget(a_accuracy)
                .partial_cmp(&accuracy_budget(b_accuracy))
                .unwrap_or(Ordering::Equal)
        });
        sources
    }
}

impl ReplacementStrategy for AccuracyReconciliationStrategy {
    fn matches(&self, target: &TargetSubDAG<'_>) -> bool {
        !self.tighter_sources(target).is_empty()
    }

    fn replacements(&self, target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG> {
        let Some((.., target_accuracy, _, _)) = bindable_accuracy_aggregate(target.root) else {
            return Vec::new();
        };
        self.tighter_sources(target)
            .into_iter()
            .map(|source| {
                let (.., source_accuracy, _, _) = bindable_accuracy_aggregate(source)
                    .expect("tighter_sources only returns bindable_accuracy_aggregate matches");
                ReplacementSubDAG {
                    strategy: "AccuracyReconciliationStrategy",
                    replacement: Replacement::Rewrite(Rc::clone(source)),
                    provenance: ReplacementProvenance::LogicalRewrite,
                    rationale: format!(
                        "reuses a near-duplicate sibling aggregate — identical intent and grouping \
                         over the same shared input, differing only in AccuracyTarget — built to a \
                         strictly tighter accuracy ({source_accuracy:?} dominates this node's own \
                         {target_accuracy:?} on both eps and delta) instead of an independent, \
                         looser-accuracy copy; every shipped sketch sizing formula is monotonic in \
                         (eps, delta), so a build sized to the tighter bound always satisfies this \
                         consumer's own looser one too — see AccuracyReconciliationStrategy's module \
                         docs for the full argument"
                    ),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::pre_asap::cse::share_common_subtrees;
    use asap_types::pre_asap::query_expr::Source;
    use asap_types::pre_asap::schema::{Column, ColumnId, DataType, Schema};

    /// `[ts(0), value(1), job(2)]`.
    /// A unique-keyed scan (`[ts]`) so `share_common_subtrees` is actually
    /// willing to hoist it — see `Schema::has_unique_key`/`cse.rs`'s own
    /// "Legality" section: a producer with no provable unique key is always
    /// inserted fresh, never hoisted, regardless of structural equality.
    fn metric_scan() -> Rc<QueryExpr> {
        Rc::new(QueryExpr::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Column::new("ts", DataType::Timestamp, false),
                    Column::new("value", DataType::Float64, false),
                    Column::new("job", DataType::Utf8, true),
                ],
                0,
                vec![vec![0]],
            ),
        })
    }

    fn agg(by: Vec<ColumnId>, intent: AggIntent, child: &Rc<QueryExpr>) -> Rc<QueryExpr> {
        Rc::new(QueryExpr::Aggregate {
            reduction: Reduction::by(by),
            measures: vec![intent],
            output_names: vec![],
            having: None,
            child: Rc::clone(child),
        })
    }

    fn quantile(q: f64, accuracy: AccuracyTarget, child: &Rc<QueryExpr>) -> Rc<QueryExpr> {
        agg(
            vec![2],
            AggIntent::Quantile {
                col: None,
                q,
                accuracy,
            },
            child,
        )
    }

    // ── dominates / strictly_tighter ─────────────────────────────────────

    #[test]
    fn a_smaller_epsilon_dominates_a_larger_one() {
        assert!(dominates(
            &AccuracyTarget::Epsilon(0.01),
            &AccuracyTarget::Epsilon(0.05)
        ));
        assert!(!dominates(
            &AccuracyTarget::Epsilon(0.05),
            &AccuracyTarget::Epsilon(0.01)
        ));
    }

    #[test]
    fn epsilon_delta_needs_both_dimensions_at_least_as_tight() {
        // Smaller epsilon but larger delta: neither Pareto-dominates.
        let a = AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.1,
        };
        let b = AccuracyTarget::EpsilonDelta {
            epsilon: 0.05,
            delta: 0.01,
        };
        assert!(!dominates(&a, &b));
        assert!(!dominates(&b, &a));
    }

    #[test]
    fn exact_never_dominates_or_is_dominated() {
        assert!(!dominates(
            &AccuracyTarget::Exact,
            &AccuracyTarget::Epsilon(0.5)
        ));
        assert!(!dominates(
            &AccuracyTarget::Epsilon(0.5),
            &AccuracyTarget::Exact
        ));
        assert!(!dominates(&AccuracyTarget::Exact, &AccuracyTarget::Exact));
    }

    #[test]
    fn equal_resolved_budgets_are_not_strictly_tighter_either_way() {
        // Same numeric (eps, delta) budget, different AccuracyTarget spelling.
        let epsilon_only = AccuracyTarget::Epsilon(0.01);
        let equivalent_epsilon_delta = AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: crate::replacement::DEFAULT_DELTA,
        };
        assert!(dominates(&epsilon_only, &equivalent_epsilon_delta));
        assert!(dominates(&equivalent_epsilon_delta, &epsilon_only));
        assert!(!strictly_tighter(&epsilon_only, &equivalent_epsilon_delta));
        assert!(!strictly_tighter(&equivalent_epsilon_delta, &epsilon_only));
    }

    // ── the issue's own scenario: quantile(0.99, x) at eps=0.01 vs eps=0.05 ─

    #[test]
    fn looser_quantile_proposes_reading_the_tighter_sibling() {
        let scan = metric_scan();
        let tight = quantile(0.99, AccuracyTarget::Epsilon(0.01), &scan);
        let loose = quantile(0.99, AccuracyTarget::Epsilon(0.05), &scan);

        let strategy = AccuracyReconciliationStrategy::new(&[Rc::clone(&tight), Rc::clone(&loose)]);

        assert!(strategy.matches(&TargetSubDAG::new(&loose)));
        let replacements = strategy.replacements(&TargetSubDAG::new(&loose));
        assert_eq!(replacements.len(), 1);
        let Replacement::Rewrite(rc) = &replacements[0].replacement else {
            panic!("expected a Rewrite candidate");
        };
        assert!(Rc::ptr_eq(rc, &tight));
        assert_eq!(
            replacements[0].provenance,
            ReplacementProvenance::LogicalRewrite
        );

        // The tighter side has nothing looser to read from: no candidate.
        assert!(!strategy.matches(&TargetSubDAG::new(&tight)));
        assert!(strategy.replacements(&TargetSubDAG::new(&tight)).is_empty());
    }

    #[test]
    fn end_to_end_search_workload_proposes_the_reconciliation_candidate() {
        // The full search_workload_with entry point, not just the strategy in
        // isolation — proves this is actually wired into the round loop
        // alongside RollupStrategy/TopKLimitReuseStrategy.
        let scan = metric_scan();
        let tight = quantile(0.99, AccuracyTarget::Epsilon(0.01), &scan);
        let loose = quantile(0.99, AccuracyTarget::Epsilon(0.05), &scan);

        let space = crate::replacement::search_workload(vec![("tight", tight), ("loose", loose)]);

        let loose_root = &space.roots[1].1;
        let loose_group = space
            .group_for(loose_root)
            .expect("loose consumer's own target has a group");
        assert!(
            loose_group.candidates.iter().any(|candidate| {
                candidate.strategy == "AccuracyReconciliationStrategy"
                    && matches!(candidate.replacement, Replacement::Rewrite(_))
            }),
            "expected an AccuracyReconciliationStrategy candidate for the looser consumer, got: \
             {:?}",
            loose_group
                .candidates
                .iter()
                .map(|c| c.strategy)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn different_columns_are_not_near_duplicates() {
        let scan = metric_scan();
        let a = agg(
            vec![2],
            AggIntent::Quantile {
                col: Some(1),
                q: 0.99,
                accuracy: AccuracyTarget::Epsilon(0.01),
            },
            &scan,
        );
        let b = agg(
            vec![2],
            AggIntent::Quantile {
                col: Some(0),
                q: 0.99,
                accuracy: AccuracyTarget::Epsilon(0.05),
            },
            &scan,
        );
        let strategy = AccuracyReconciliationStrategy::new(&[Rc::clone(&a), Rc::clone(&b)]);
        assert!(!strategy.matches(&TargetSubDAG::new(&b)));
    }

    #[test]
    fn different_grouping_is_not_a_near_duplicate() {
        let scan = metric_scan();
        let tight = quantile(0.99, AccuracyTarget::Epsilon(0.01), &scan);
        let loose = agg(
            vec![],
            AggIntent::Quantile {
                col: None,
                q: 0.99,
                accuracy: AccuracyTarget::Epsilon(0.05),
            },
            &scan,
        );
        let strategy = AccuracyReconciliationStrategy::new(&[Rc::clone(&tight), Rc::clone(&loose)]);
        assert!(!strategy.matches(&TargetSubDAG::new(&loose)));
    }

    #[test]
    fn exact_accuracy_is_never_reconciled() {
        let scan = metric_scan();
        let exact = quantile(0.99, AccuracyTarget::Exact, &scan);
        let approx = quantile(0.99, AccuracyTarget::Epsilon(0.05), &scan);
        let strategy =
            AccuracyReconciliationStrategy::new(&[Rc::clone(&exact), Rc::clone(&approx)]);
        assert!(!strategy.matches(&TargetSubDAG::new(&approx)));
        assert!(!strategy.matches(&TargetSubDAG::new(&exact)));
    }

    // ── exact structural equality / share_common_subtrees is unchanged ────

    #[test]
    fn share_common_subtrees_still_never_merges_differing_accuracy() {
        // The additive guarantee this issue explicitly must not violate:
        // pre-ASAP CSE's own exact-equality merge stays exact. Two
        // aggregates differing only in `accuracy` must come back as two
        // distinct `Rc`s, not one shared `Rc` — AccuracyReconciliationStrategy
        // is the *only* place cross-accuracy sharing gets proposed, never
        // `share_common_subtrees` itself.
        let scan = metric_scan();
        let a = (*quantile(0.99, AccuracyTarget::Epsilon(0.01), &scan)).clone();
        let b = (*quantile(0.99, AccuracyTarget::Epsilon(0.05), &scan)).clone();

        let roots = share_common_subtrees(vec![("a", a), ("b", b)]);
        assert!(
            !Rc::ptr_eq(&roots[0].1, &roots[1].1),
            "share_common_subtrees must not merge aggregates with different AccuracyTarget"
        );
        assert_ne!(
            roots[0].1, roots[1].1,
            "the two aggregates really are structurally different (accuracy differs)"
        );

        // The identical scan child, though, is still shared exactly as
        // before — this module changes nothing about that.
        let QueryExpr::Aggregate { child: child_a, .. } = roots[0].1.as_ref() else {
            panic!("expected an Aggregate root");
        };
        let QueryExpr::Aggregate { child: child_b, .. } = roots[1].1.as_ref() else {
            panic!("expected an Aggregate root");
        };
        assert!(Rc::ptr_eq(child_a, child_b));
    }

    #[test]
    fn identical_accuracy_still_merges_via_ordinary_cse() {
        // Sanity check the fixture itself: truly identical aggregates
        // (same accuracy too) still merge via share_common_subtrees's own
        // exact equality — unrelated to this module, but pins the contrast
        // with the test above.
        let scan = metric_scan();
        let a = (*quantile(0.99, AccuracyTarget::Epsilon(0.01), &scan)).clone();
        let b = (*quantile(0.99, AccuracyTarget::Epsilon(0.01), &scan)).clone();

        let roots = share_common_subtrees(vec![("a", a), ("b", b)]);
        assert!(Rc::ptr_eq(&roots[0].1, &roots[1].1));
    }
}
