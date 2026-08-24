//! `TargetSubDAG` / `ReplacementSubDAG` / `ReplacementStrategy` — the
//! candidate-replacement vocabulary `docs/design_docs/asap_aware_mapping.md` stubs out
//! under "Key concepts (not yet implemented)", implemented for real (issue
//! #251, part of #33).
//!
//! ## This is where every `Implementation` gets decided — and the only place bound output comes from
//!
//! [`implementation::implementations_for_with`] is the one place this crate
//! enumerates every valid [`Implementation`] for an [`AggIntent`], exhaustive
//! and ranked (most-preferred first via a [`CostModel`]). This module wraps
//! that list into the exhaustive-candidate shape a future search needs, and
//! is the *only* public way to get bound output for a target — see
//! "Selection" below.
//!
//! - [`TargetSubDAG`] — a reference to a pre-ASAP [`QueryExpr`] node that is a
//!   candidate for replacement, plus how many places in the workload already
//!   reference it (its `consumer_count`) — the one piece of cross-node
//!   context [`SharedSubtreeStrategy`] needs that a bare node reference alone
//!   doesn't carry.
//! - [`ReplacementSubDAG`] — one candidate replacement for a `TargetSubDAG`:
//!   either a fully bound [`SummaryNode`] or a pre-ASAP [`QueryExpr`] rewrite
//!   (still logical, structurally different from the target but semantically
//!   equivalent) — see [`Replacement`] — plus a human-readable `rationale`.
//! - [`ReplacementStrategy`] — `matches` + `replacements`, the same
//!   extension-point shape [`CostModel`] and [`Matcher`](crate::implementation::Matcher)
//!   already use in this crate (and the same shape issue #33's
//!   applicability-rule framework, PR #247, uses for its own
//!   `ApplicabilityRule`): a new replacement source is a new
//!   `impl ReplacementStrategy`, not a restructuring of this trait or of any
//!   existing strategy. `replacements` is **exhaustive, not ranked, not
//!   filtered** — reporting "every valid candidate" is core's job; picking
//!   the best one is left to the caller (see "Selection" below).
//!
//! ## Selection: this crate reports every candidate; a caller keeps what it wants
//!
//! [`SketchAlgorithmStrategy::replacements`] builds a `TargetSubDAG` for the
//! node being replaced, then enumerates [`implementation::implementations_for_with`]'s
//! ranked list, calling [`bind::bind_with_implementation`] once per
//! candidate — given an *already-decided* `Implementation`, turn it into a
//! `SummaryNode`. Every candidate comes back; nothing is discarded here. A
//! caller that wants one executable answer takes the first
//! (`cost_model`-preferred) entry itself (`.into_iter().next()`) — that
//! "keep the head" step now lives entirely on the calling side, not behind
//! a second module-level entry point. `bind.rs`'s own
//! [`bind::implement_workload`]/[`bind::implement_workload_with`] are the
//! one place inside this crate that still performs that take-first step
//! internally, because workload-wide CSE memoization needs one canonical
//! decision per shared root to key sharing on (see `bind.rs`'s own module
//! docs). Every other caller goes through `SketchAlgorithmStrategy::replacements`
//! directly and decides for itself.
//!
//! This means an ordinary single-target bind now sizes and fully constructs
//! *every* sketch candidate at every sketch-capable node (not just the one a
//! caller keeps) — a deliberate tradeoff, made so there is exactly one place
//! in this crate that decides what an `AggIntent` may become, at the cost of
//! extra work per bind proportional to each node's own candidate count.
//!
//! ## The two strategies, and why these two
//!
//! - [`SketchAlgorithmStrategy`] wraps [`implementation::implementations_for_with`]'s
//!   exhaustive, ranked list directly: for the same bindable-`Aggregate`
//!   shape this crate binds (single intent, no `HAVING`), every entry
//!   becomes its own bound candidate.
//! - [`SharedSubtreeStrategy`] wraps
//!   `asap_types::pre_asap::cse::share_common_subtrees`'s sharing decision.
//!   Wherever a [`TargetSubDAG`] already has two or more consumers (i.e.
//!   `share_common_subtrees` already collapsed two or more workload
//!   locations onto the same `Rc<QueryExpr>` — PR #247's own
//!   `SharedSubexpressionRule` traversal/dedup logic, over in the
//!   applicability-rule framework, does the identical discovery; this
//!   module's own tests reuse the same dedup logic to build realistic
//!   fixtures), it reports the two-way candidate CSE's own detection pass
//!   deliberately declines to pick between on its own: build once and share
//!   the already-interned subtree, or build it independently at each
//!   consumer. [`crate::cost_model::CostModel::cse_share_decision`] is where
//!   that choice actually gets made *today* (a fixed comparison, not a
//!   search) — this strategy exposes the same two-way choice as an explicit,
//!   inspectable pair of candidates instead of a cost model's already-decided
//!   boolean.
//!
//! ## Non-goals (tracked separately, not attempted here)
//!
//! - **No search/selection-across-a-whole-plan logic.** `docs/design_docs/asap_aware_mapping.md`'s
//!   replacement-plan-searching pseudocode — trying every `ReplacementStrategy`
//!   against every candidate plan, deduplicating, iterating to a fixpoint,
//!   then ranking by a `CostModel` — is a Cascades/Volcano-style search
//!   engine, tracked as a separate follow-up. Taking the first candidate off
//!   [`SketchAlgorithmStrategy::replacements`] is a single-node stand-in for
//!   that, not the real thing: it never compares whole candidate *plans*,
//!   only one node's own candidates against each other via
//!   `cost_model.rank_candidates`.
//! - **No workload-wide `TargetSubDAG` discovery pass.** Finding every
//!   candidate node in a whole workload (walking every root, deduplicating by
//!   `Rc` identity, computing real consumer counts) is exactly what PR #247's
//!   `SharedSubexpressionRule`/`SketchApplicabilityRule` traversals already
//!   do — reusable, but wiring it up to feed this module's strategies
//!   automatically is part of the same future search engine, not this issue.
//!   This module's own tests build `TargetSubDAG`s directly, the same
//!   hand-rolled-fixture style `bind.rs`/`implementation.rs`/`cost_model.rs`'s own
//!   tests already use.
//! - **[`implementation::implementations_for_with`]'s own outward-facing
//!   behavior is unchanged.** Same inputs still produce the same exhaustive,
//!   ranked list. What changed is who calls it and how much of the result
//!   they keep — see "Selection" above.

use std::rc::Rc;

use asap_types::post_asap::SummaryNode;
use asap_types::pre_asap::agg_intent::AggIntent;
use asap_types::pre_asap::query_expr::QueryExpr;

use crate::bind::{bind_with_implementation, bindable_intent};
use crate::cost_model::{CostModel, DefaultCostModel};
use crate::implementation::{implementations_for_with, Implementation};

/// A pre-ASAP sub-DAG a [`ReplacementStrategy`] knows how to replace.
///
/// `root` is a reference into the workload's own [`QueryExpr`] tree (an
/// `Rc<QueryExpr>`, the same currency [`bind::implement_workload`] and
/// `asap_types::pre_asap::cse::share_common_subtrees` already thread through
/// this crate's public API — not a bare `&QueryExpr` — so a strategy that
/// needs the node's own `Rc` identity, not just its shape, has it available
/// without the caller re-deriving it).
///
/// `consumer_count` is how many locations across the workload already
/// reference this exact `Rc` — 1 for an ordinary single-use node, 2+ when the
/// caller already ran `share_common_subtrees` and found this subtree shared.
/// A strategy that only cares about `root`'s own shape (e.g.
/// [`SketchAlgorithmStrategy`]) can ignore it entirely; [`SharedSubtreeStrategy`]
/// is the one strategy that consults it. Computing a *real* consumer count
/// across a whole workload is a traversal this module deliberately does not
/// own (see the module docs' "Non-goals") — [`TargetSubDAG::new`] defaults it
/// to `1`, the safe assumption for a caller inspecting one node in isolation.
#[derive(Debug, Clone, Copy)]
pub struct TargetSubDAG<'a> {
    pub root: &'a Rc<QueryExpr>,
    pub consumer_count: usize,
}

impl<'a> TargetSubDAG<'a> {
    /// A target assumed to have exactly one consumer — the common case for a
    /// caller that isn't already tracking cross-workload sharing.
    pub fn new(root: &'a Rc<QueryExpr>) -> Self {
        Self {
            root,
            consumer_count: 1,
        }
    }

    /// A target with an explicit `consumer_count` — for a caller that already
    /// knows (e.g. from `share_common_subtrees`, or from this module's own
    /// test helpers) how many workload locations reference `root`.
    pub fn with_consumer_count(root: &'a Rc<QueryExpr>, consumer_count: usize) -> Self {
        Self {
            root,
            consumer_count,
        }
    }
}

/// What a [`ReplacementSubDAG`] actually substitutes a [`TargetSubDAG`] with.
///
/// Generalizes [`implementation::implementations_for_with`]'s two possible
/// *kinds* of answer — a post-ASAP binding decision, or a still-pre-ASAP
/// structural alternative — into "one candidate among several", each with
/// its own [`ReplacementSubDAG`].
#[derive(Debug, Clone)]
pub enum Replacement {
    /// A fully bound post-ASAP summary decision — the same [`SummaryNode`]
    /// shape [`bind::bind_with_implementation`] produces, for one particular
    /// candidate realization of the target.
    Summary(Rc<SummaryNode>),
    /// A pre-ASAP rewrite: still a logical [`QueryExpr`], structurally
    /// different from the target's own `root` (e.g. sharing vs. not sharing
    /// a subtree) but semantically equivalent to it.
    Rewrite(Rc<QueryExpr>),
}

/// One candidate replacement for a [`TargetSubDAG`], plus a human-readable
/// `rationale` explaining why it's a valid candidate (meant for a
/// report/log/debugging a search engine's choices, not machine parsing —
/// the same role an `ApplicabilityFinding`'s `reason` field plays for
/// applicability findings, over in PR #247's applicability-rule framework).
#[derive(Debug, Clone)]
pub struct ReplacementSubDAG {
    pub replacement: Replacement,
    pub rationale: String,
}

/// A replacement strategy: given a [`TargetSubDAG`], does this strategy have
/// an opinion on it at all (`matches`), and if so, every semantically valid
/// replacement (`replacements`)?
///
/// The extension point this module exists for — the same shape
/// [`CostModel`] and [`crate::implementation::Matcher`] already use elsewhere in
/// this crate: a new replacement source is a new `impl ReplacementStrategy`,
/// no restructuring of this trait or any existing strategy required.
///
/// `replacements` is only meaningful when `matches` would return `true` for
/// the same target; both [`SketchAlgorithmStrategy`] and [`SharedSubtreeStrategy`]
/// return an empty `Vec` rather than panicking when called on a target they
/// don't match, so a caller that skips the `matches` check first still gets a
/// safe (merely uninformative) answer instead of a crash.
pub trait ReplacementStrategy {
    /// Does this strategy have any replacement to offer for `target`?
    fn matches(&self, target: &TargetSubDAG<'_>) -> bool;

    /// Every valid replacement for `target` — not ranked, not filtered.
    /// Reporting "every valid candidate" is this method's whole job; picking
    /// the best one is a [`CostModel`]'s job, out of scope here.
    fn replacements(&self, target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG>;
}

// ── SketchAlgorithmStrategy ─────────────────────────────────────────────────

/// A single static instance so [`SketchAlgorithmStrategy::default_cost_model`]
/// can hand out a `&'static dyn CostModel` without heap-allocating one —
/// `DefaultCostModel` is a unit struct with no state, so one instance serves
/// every caller (same pattern `applicability::SketchApplicabilityRule` uses).
static DEFAULT_COST_MODEL: DefaultCostModel = DefaultCostModel;

/// Wraps [`implementation::implementations_for_with`]'s exhaustive, ranked
/// list directly: for a bindable `Aggregate`, every valid candidate summary
/// realization as its own [`ReplacementSubDAG`].
///
/// Ranked (only to *order the enumeration*, never to drop a candidate) via a
/// [`CostModel`] — [`DefaultCostModel`] unless constructed with
/// [`SketchAlgorithmStrategy::new`] — so a deployment-specific cost model's
/// other hooks (`size_params`, `realize_extension`, `readout_extension`) are
/// still consulted while binding each candidate.
pub struct SketchAlgorithmStrategy<'a> {
    cost_model: &'a dyn CostModel,
}

impl SketchAlgorithmStrategy<'static> {
    /// A strategy that ranks/binds via the built-in [`DefaultCostModel`] —
    /// what a deployment gets with no custom cost model plugged in.
    pub fn default_cost_model() -> Self {
        Self {
            cost_model: &DEFAULT_COST_MODEL,
        }
    }
}

impl<'a> SketchAlgorithmStrategy<'a> {
    /// A strategy that ranks/binds via `cost_model` instead of the built-in
    /// static preference order — the same customization point
    /// [`implementation::implementations_for_with`] already offers.
    pub fn new(cost_model: &'a dyn CostModel) -> Self {
        Self { cost_model }
    }
}

impl ReplacementStrategy for SketchAlgorithmStrategy<'_> {
    fn matches(&self, target: &TargetSubDAG<'_>) -> bool {
        bindable_intent(target.root).is_some()
    }

    fn replacements(&self, target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG> {
        let Some(intent) = bindable_intent(target.root) else {
            return Vec::new();
        };
        // `implementations_for_with` is already exhaustive and ranked — no
        // separate dispatch needed here. Only `Sketch` has more than one
        // candidate in practice (every other variant's own dispatch produces
        // exactly one `Implementation`), but this loop doesn't need to know
        // that; it just binds whatever the list contains.
        implementations_for_with(intent, self.cost_model)
            .into_iter()
            .filter_map(|implementation| {
                let rationale = describe_implementation(intent, &implementation);
                let node =
                    bind_with_implementation(target.root, implementation, self.cost_model).ok()?;
                Some(ReplacementSubDAG {
                    replacement: Replacement::Summary(node),
                    rationale,
                })
            })
            .collect()
    }
}

/// A human-readable rationale for one candidate `Implementation`, for
/// [`ReplacementSubDAG::rationale`] text.
fn describe_implementation(intent: &AggIntent, implementation: &Implementation) -> String {
    match implementation {
        Implementation::Sketch(kind) => format!(
            "{} realizes as a {:?} sketch — one of implementation::summary_candidates' \
             alternatives for this intent \
             (asap_aware_mapping::implementation::implementations_for_with)",
            describe_intent(intent),
            kind.algorithm()
        ),
        Implementation::ExactAggregate { kind, .. } => format!(
            "{} realizes as an exact {kind:?} accumulator — the only realization \
             implementation::implementations_for_with produces for this intent (no approximate \
             candidate applies)",
            describe_intent(intent)
        ),
        Implementation::PassThrough => format!(
            "{} has no summary realization and stays a logical pass-through — the \
             only realization implementation::implementations_for_with produces for this intent",
            describe_intent(intent)
        ),
        Implementation::Sample { kind, .. } => format!(
            "{} realizes as a {kind:?} sample — the only realization the plugged-in \
             CostModel produced for this intent",
            describe_intent(intent)
        ),
        Implementation::Wavelet { kind, .. } => format!(
            "{} realizes as a {kind:?} wavelet transform — the only realization the \
             plugged-in CostModel produced for this intent",
            describe_intent(intent)
        ),
        Implementation::StatModel { kind, .. } => format!(
            "{} realizes as a {kind:?} statistical model — the only realization the \
             plugged-in CostModel produced for this intent",
            describe_intent(intent)
        ),
    }
}

/// A short human-readable label for an `AggIntent`, for
/// [`ReplacementSubDAG::rationale`] text. Not exhaustive by design (unlike
/// this crate's other `AggIntent` matches, e.g.
/// [`implementation::implementations_for_with`]'s) — this is prose for a
/// rationale string, not a decision, so an unlisted variant just falls back
/// to its `Debug` tag rather than forcing every future intent to be named
/// here too (same rationale, and same shape, as
/// `applicability`'s own private `describe_intent`).
fn describe_intent(intent: &AggIntent) -> String {
    match intent {
        AggIntent::Quantile { q, .. } => format!("quantile(q={q})"),
        AggIntent::Cardinality { .. } => "cardinality (distinct count)".to_string(),
        AggIntent::TopK { k, .. } => format!("top-{k} heavy-hitters"),
        AggIntent::Count { .. } => "count".to_string(),
        other => format!("{other:?}"),
    }
}

// ── SharedSubtreeStrategy ────────────────────────────────────────────────

/// Wraps `asap_types::pre_asap::cse::share_common_subtrees`'s sharing
/// decision as an explicit candidate pair, wherever a [`TargetSubDAG`]
/// already has two or more consumers.
///
/// This strategy does not decide sharing itself, nor does it discover which
/// nodes are shared — by the time a caller builds a `TargetSubDAG` with
/// `consumer_count >= 2`, `share_common_subtrees` has already made that
/// (legality-gated, `PartialEq`-checked) call; PR #247's
/// `SharedSubexpressionRule` traversal discovers real consumer counts across
/// a workload the same way (this module's own tests reuse the identical
/// dedup logic to build realistic fixtures — see the module docs'
/// "Non-goals" on why that traversal isn't itself part of this strategy).
/// This strategy only reframes "two or more consumers already share this
/// `Rc`" as the two-way choice a downstream cost model (today,
/// [`CostModel::cse_share_decision`]) picks between: build once and share, or
/// build independently at each consumer.
pub struct SharedSubtreeStrategy;

impl ReplacementStrategy for SharedSubtreeStrategy {
    fn matches(&self, target: &TargetSubDAG<'_>) -> bool {
        target.consumer_count >= 2
    }

    fn replacements(&self, target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG> {
        if target.consumer_count < 2 {
            return Vec::new();
        }
        let count = target.consumer_count;
        vec![
            ReplacementSubDAG {
                // The already-interned `Rc` itself: reusing it verbatim *is*
                // "build once and share" — no new node to construct.
                replacement: Replacement::Rewrite(Rc::clone(target.root)),
                rationale: format!(
                    "build once and share: share_common_subtrees already interned this \
                     subtree once and reused it across {count} consumers — one build can \
                     answer all of them instead of computing it {count} times"
                ),
            },
            ReplacementSubDAG {
                // A structurally-identical but freshly-allocated `Rc`: same
                // value (`PartialEq`), deliberately *not* the same pointer,
                // representing "undo the sharing and recompute independently".
                replacement: Replacement::Rewrite(Rc::new((**target.root).clone())),
                rationale: format!(
                    "build independently: undo the sharing share_common_subtrees found and \
                     recompute this subtree separately at each of its {count} consumers — \
                     worth it only when independence outweighs the shared-maintenance cost, \
                     a CostModel's call (e.g. CostModel::cse_share_decision) and not this \
                     strategy's"
                ),
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::post_asap::SketchAlgorithm;
    use asap_types::pre_asap::agg_intent::{default_cardinality, default_quantile};
    use asap_types::pre_asap::query_expr::{Reduction, Source};
    use asap_types::pre_asap::schema::{Column, DataType, Schema};
    use asap_types::types::AccuracyTarget;
    use std::collections::HashMap;

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

    fn agg(by: Vec<usize>, intent: AggIntent, child: QueryExpr) -> QueryExpr {
        QueryExpr::Aggregate {
            reduction: Reduction::by(by),
            measures: vec![intent],
            output_names: vec![],
            having: None,
            child: Rc::new(child),
        }
    }

    // ── SketchAlgorithmStrategy ─────────────────────────────────────────────

    #[test]
    fn matches_a_bindable_aggregate() {
        let q = Rc::new(agg(vec![2], default_quantile(0.99), metric_scan(&["job"])));
        let target = TargetSubDAG::new(&q);
        assert!(SketchAlgorithmStrategy::default_cost_model().matches(&target));
    }

    #[test]
    fn does_not_match_a_multi_intent_or_having_aggregate() {
        let strategy = SketchAlgorithmStrategy::default_cost_model();

        let multi = Rc::new(QueryExpr::Aggregate {
            reduction: Reduction::by(vec![2]),
            measures: vec![AggIntent::Sum { col: None }, AggIntent::Avg { col: None }],
            output_names: vec![],
            having: None,
            child: Rc::new(metric_scan(&["job"])),
        });
        let target = TargetSubDAG::new(&multi);
        assert!(!strategy.matches(&target));
        assert!(strategy.replacements(&target).is_empty());

        let mut having_q = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        if let QueryExpr::Aggregate { having, .. } = &mut having_q {
            *having = Some(asap_types::pre_asap::query_expr::Predicate(Rc::new(
                QueryExpr::Literal(asap_types::pre_asap::expr_ir::ScalarValue::Boolean(true)),
            )));
        }
        let having_q = Rc::new(having_q);
        let target = TargetSubDAG::new(&having_q);
        assert!(!strategy.matches(&target));
        assert!(strategy.replacements(&target).is_empty());
    }

    #[test]
    fn does_not_match_a_non_aggregate_node() {
        let scan = Rc::new(metric_scan(&["job"]));
        let target = TargetSubDAG::new(&scan);
        assert!(!SketchAlgorithmStrategy::default_cost_model().matches(&target));
        assert!(SketchAlgorithmStrategy::default_cost_model()
            .replacements(&target)
            .is_empty());
    }

    #[test]
    fn approximate_quantile_enumerates_every_summary_candidate() {
        // Quantile's candidate list is [Kll, DDSketch] (implementation::summary_candidates) —
        // every entry must come back as its own bound SummaryNode candidate,
        // not just Kll (the CostModel-ranked head implementations_for_with commits to).
        let q = Rc::new(agg(vec![2], default_quantile(0.99), metric_scan(&["job"])));
        let target = TargetSubDAG::new(&q);
        let replacements = SketchAlgorithmStrategy::default_cost_model().replacements(&target);
        assert_eq!(
            replacements.len(),
            2,
            "expected 2 candidates, got {replacements:?}"
        );

        let kinds: Vec<SketchAlgorithm> = replacements
            .iter()
            .map(|r| match &r.replacement {
                Replacement::Summary(node) => summary_family_algorithm(node),
                Replacement::Rewrite(_) => panic!("expected a Summary replacement"),
            })
            .collect();
        assert!(kinds.contains(&SketchAlgorithm::Kll), "{kinds:?}");
        assert!(kinds.contains(&SketchAlgorithm::DDSketch), "{kinds:?}");
        assert!(
            replacements.iter().all(|r| !r.rationale.is_empty()),
            "every candidate must carry a rationale"
        );
    }

    #[test]
    fn cardinality_enumerates_all_three_summary_candidates() {
        let q = Rc::new(agg(vec![2], default_cardinality(), metric_scan(&["job"])));
        let target = TargetSubDAG::new(&q);
        let replacements = SketchAlgorithmStrategy::default_cost_model().replacements(&target);
        let kinds: Vec<SketchAlgorithm> = replacements
            .iter()
            .map(|r| match &r.replacement {
                Replacement::Summary(node) => summary_family_algorithm(node),
                Replacement::Rewrite(_) => panic!("expected a Summary replacement"),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                SketchAlgorithm::Hll,
                SketchAlgorithm::Theta,
                SketchAlgorithm::Kmv
            ],
            "expected every implementation::summary_candidates entry for Cardinality"
        );
    }

    #[test]
    fn exact_accuracy_target_yields_exactly_one_pass_through_candidate() {
        // Exact quantile has no sketch candidate at all — implementations_for_with
        // produces PassThrough, the only option, so exactly one candidate.
        let intent = AggIntent::Quantile {
            col: None,
            q: 0.99,
            accuracy: AccuracyTarget::Exact,
        };
        let q = Rc::new(agg(vec![2], intent, metric_scan(&["job"])));
        let target = TargetSubDAG::new(&q);
        let replacements = SketchAlgorithmStrategy::default_cost_model().replacements(&target);
        assert_eq!(replacements.len(), 1, "{replacements:?}");
        assert!(matches!(
            &replacements[0].replacement,
            Replacement::Summary(node) if matches!(
                node.expr,
                asap_types::post_asap::SummaryExpr::Logical(_)
            )
        ));
        assert!(replacements[0].rationale.contains("only realization"));
    }

    #[test]
    fn exact_mergeable_intent_yields_exactly_one_accumulator_candidate() {
        let q = Rc::new(agg(
            vec![2],
            AggIntent::Sum { col: None },
            metric_scan(&["job"]),
        ));
        let target = TargetSubDAG::new(&q);
        let replacements = SketchAlgorithmStrategy::default_cost_model().replacements(&target);
        assert_eq!(replacements.len(), 1, "{replacements:?}");
        assert!(matches!(
            &replacements[0].replacement,
            Replacement::Summary(node) if matches!(
                node.expr,
                asap_types::post_asap::SummaryExpr::SummaryAgg { .. }
            )
        ));
    }

    /// A custom `CostModel` doesn't change *which* candidates are enumerated
    /// (still every `summary_candidates` entry) — only which one
    /// `implementations_for_with` itself would prefer first, and how each
    /// candidate's own params are sized.
    struct PreferDDSketch;
    impl CostModel for PreferDDSketch {
        fn rank_candidates(
            &self,
            _intent: &AggIntent,
            candidates: &[SketchAlgorithm],
        ) -> Vec<SketchAlgorithm> {
            let mut v = candidates.to_vec();
            if let Some(pos) = v.iter().position(|k| *k == SketchAlgorithm::DDSketch) {
                let dd = v.remove(pos);
                v.insert(0, dd);
            }
            v
        }
    }

    #[test]
    fn custom_cost_model_still_enumerates_every_candidate_not_just_its_own_pick() {
        let q = Rc::new(agg(vec![2], default_quantile(0.99), metric_scan(&["job"])));
        let target = TargetSubDAG::new(&q);
        let custom = PreferDDSketch;
        let replacements = SketchAlgorithmStrategy::new(&custom).replacements(&target);
        let kinds: Vec<SketchAlgorithm> = replacements
            .iter()
            .map(|r| match &r.replacement {
                Replacement::Summary(node) => summary_family_algorithm(node),
                Replacement::Rewrite(_) => panic!("expected a Summary replacement"),
            })
            .collect();
        assert!(kinds.contains(&SketchAlgorithm::Kll));
        assert!(kinds.contains(&SketchAlgorithm::DDSketch));
        assert_eq!(kinds.len(), 2);
    }

    /// Enumerating candidates for the *target* node must only steer that
    /// node's own decision — a nested aggregate underneath it still gets its
    /// own independent (`cost_model`-ranked) enumeration, not whatever the
    /// caller happened to pick for the outer target. This is the behavior
    /// [`bind::bind_with_implementation`]'s recursion (via
    /// `bind::select_and_bind`) gets for free: only the top node's
    /// `Implementation` is ever forced from outside; the child is always
    /// re-enumerated fresh.
    #[test]
    fn enumerating_the_targets_candidates_does_not_leak_into_a_nested_aggregate() {
        // outer: quantile(0.99, ...) over inner: quantile(0.5, m) — both
        // Quantile, so both share the [Kll, DDSketch] candidate list.
        let inner = agg(vec![2], default_quantile(0.5), metric_scan(&["job"]));
        let outer = Rc::new(agg(vec![], default_quantile(0.99), inner));
        let target = TargetSubDAG::new(&outer);
        let replacements = SketchAlgorithmStrategy::default_cost_model().replacements(&target);

        let ddsketch = replacements
            .iter()
            .find(|r| {
                matches!(&r.replacement, Replacement::Summary(node)
                    if summary_family_algorithm(node) == SketchAlgorithm::DDSketch)
            })
            .expect("the outer target's DDSketch candidate must be present");
        let Replacement::Summary(node) = &ddsketch.replacement else {
            unreachable!("filtered on Replacement::Summary above");
        };
        assert_eq!(
            summary_family_algorithm(node),
            SketchAlgorithm::DDSketch,
            "the outer (target) node must be the DDSketch candidate"
        );

        let asap_types::post_asap::SummaryExpr::SummaryEstimate { summary_input, .. } = &node.expr
        else {
            panic!("expected SummaryEstimate root, got {:?}", node.expr);
        };
        let asap_types::post_asap::SummaryExpr::SummaryAgg { child, .. } = &summary_input.expr
        else {
            panic!("expected SummaryAgg, got {:?}", summary_input.expr);
        };
        assert_eq!(
            summary_family_algorithm(child),
            SketchAlgorithm::Kll,
            "the nested inner aggregate must still get the cost-model-ranked \
             default (Kll), not inherit the outer target's DDSketch candidate"
        );
    }

    /// The `SummaryFamilyType`'s committed `SketchAlgorithm`, from the top
    /// `SummaryAgg` reachable under a (possibly `SummaryEstimate`-wrapped)
    /// bound root.
    fn summary_family_algorithm(node: &SummaryNode) -> SketchAlgorithm {
        match &node.expr {
            asap_types::post_asap::SummaryExpr::SummaryEstimate { summary_input, .. } => {
                summary_family_algorithm(summary_input)
            }
            asap_types::post_asap::SummaryExpr::SummaryAgg { family, .. } => match family {
                asap_types::post_asap::SummaryFamilyType::Sketch(kind) => kind.algorithm().clone(),
                other => panic!("expected a Sketch family, got {other:?}"),
            },
            other => panic!("expected SummaryAgg/SummaryEstimate, got {other:?}"),
        }
    }

    // ── SharedSubtreeStrategy ────────────────────────────────────────────

    #[test]
    fn does_not_match_a_single_consumer_target() {
        let q = Rc::new(agg(
            vec![2],
            AggIntent::Sum { col: None },
            metric_scan(&["job"]),
        ));
        let target = TargetSubDAG::new(&q);
        assert_eq!(target.consumer_count, 1);
        assert!(!SharedSubtreeStrategy.matches(&target));
        assert!(SharedSubtreeStrategy.replacements(&target).is_empty());
    }

    #[test]
    fn two_or_more_consumers_yields_the_share_vs_independent_pair() {
        let q = Rc::new(agg(
            vec![2],
            AggIntent::Sum { col: None },
            metric_scan(&["job"]),
        ));
        let target = TargetSubDAG::with_consumer_count(&q, 2);
        assert!(SharedSubtreeStrategy.matches(&target));

        let replacements = SharedSubtreeStrategy.replacements(&target);
        assert_eq!(replacements.len(), 2, "{replacements:?}");

        let shared = match &replacements[0].replacement {
            Replacement::Rewrite(rc) => rc,
            other => panic!("expected a Rewrite replacement, got {other:?}"),
        };
        assert!(
            Rc::ptr_eq(shared, &q),
            "the 'build once and share' candidate must be the same Rc as the target"
        );
        assert!(replacements[0].rationale.contains("build once and share"));

        let independent = match &replacements[1].replacement {
            Replacement::Rewrite(rc) => rc,
            other => panic!("expected a Rewrite replacement, got {other:?}"),
        };
        assert!(
            !Rc::ptr_eq(independent, &q),
            "the 'build independently' candidate must be a distinct Rc from the target"
        );
        assert_eq!(
            **independent, *q,
            "the 'build independently' candidate must still be structurally identical"
        );
        assert!(replacements[1].rationale.contains("build independently"));
    }

    #[test]
    fn three_consumers_are_reported_verbatim_in_both_rationales() {
        let q = Rc::new(agg(
            vec![2],
            AggIntent::Sum { col: None },
            metric_scan(&["job"]),
        ));
        let target = TargetSubDAG::with_consumer_count(&q, 3);
        let replacements = SharedSubtreeStrategy.replacements(&target);
        assert!(replacements[0].rationale.contains('3'));
        assert!(replacements[1].rationale.contains('3'));
    }

    /// Builds realistic multi-consumer `TargetSubDAG`s the same way
    /// `applicability::SharedSubexpressionRule`'s `register_site`/
    /// `walk_rc_children` traversal does: dedup by `Rc::as_ptr`, walking
    /// only the relational-skeleton operator children
    /// `asap_types::pre_asap::cse::share_common_subtrees` itself scopes to,
    /// so a shared node nested below another shared node is only ever
    /// counted at the highest (maximal) point sharing starts. Test-only:
    /// this module deliberately does not ship a workload-wide discovery
    /// pass of its own (see the module docs' "Non-goals").
    fn count_consumers(roots: &[Rc<QueryExpr>]) -> HashMap<*const QueryExpr, usize> {
        fn walk(node: &Rc<QueryExpr>, counts: &mut HashMap<*const QueryExpr, usize>) {
            let ptr = Rc::as_ptr(node);
            let already_visited = counts.contains_key(&ptr);
            *counts.entry(ptr).or_insert(0) += 1;
            if !already_visited {
                walk_children(node, counts);
            }
        }
        fn walk_children(node: &QueryExpr, counts: &mut HashMap<*const QueryExpr, usize>) {
            use QueryExpr::*;
            match node {
                Scan { .. } | PromqlScalarBridge(_) | QueryTimestamp => {}
                PromqlVectorFromScalar(c) | PromqlScalarFromVector(c) => walk(c, counts),
                PromqlRelabel { child, .. }
                | PromqlInfoEnrich { child, .. }
                | PromqlSeriesSample { child, .. }
                | Filter { child, .. }
                | Project { child, .. }
                | Aggregate { child, .. }
                | Dedup { child, .. }
                | PromqlSubquery { child, .. }
                | TimeRange { child, .. }
                | TimeShift { child, .. }
                | SQLWindowFunc { child, .. }
                | Sort { child, .. }
                | Limit { child, .. } => walk(child, counts),
                Concat { children } => {
                    for c in children {
                        walk_children(c, counts);
                    }
                }
                Join { left, right, .. } | SetOp { left, right, .. } => {
                    walk(left, counts);
                    walk(right, counts);
                }
                BinaryOp { lhs, rhs, .. } => {
                    walk(lhs, counts);
                    walk(rhs, counts);
                }
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
                | Case { .. } => {}
            }
        }

        let mut counts = HashMap::new();
        for root in roots {
            walk(root, &mut counts);
        }
        counts
    }

    #[test]
    fn realistic_cse_output_produces_a_two_consumer_target() {
        // Two workload roots that `share_common_subtrees` collapses onto one
        // Rc (mirrors `applicability`'s and `cse`'s own fixtures): a grouped
        // Sum aggregate over the same scan, built independently at each root.
        let a = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let b = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let shared = asap_types::pre_asap::cse::share_common_subtrees(vec![("a", a), ("b", b)]);
        let [(_, ra), (_, rb)] = shared.as_slice() else {
            panic!("expected 2 roots");
        };
        assert!(Rc::ptr_eq(ra, rb), "fixture sanity: the two roots merged");

        let roots: Vec<Rc<QueryExpr>> = shared.into_iter().map(|(_, rc)| rc).collect();
        let counts = count_consumers(&roots);
        let count = counts[&Rc::as_ptr(&roots[0])];
        assert_eq!(count, 2);

        let target = TargetSubDAG::with_consumer_count(&roots[0], count);
        assert!(SharedSubtreeStrategy.matches(&target));
        assert_eq!(SharedSubtreeStrategy.replacements(&target).len(), 2);
    }
}
