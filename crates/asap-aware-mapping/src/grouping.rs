//! `GroupingStrategy` (issue #256, part of #33): the axis deciding whether a
//! grouped aggregate's summary state is built as one independent instance
//! per `by` subpopulation (today's only, implicit behavior) or as one
//! shared Hydra-family structure serving all of them — orthogonal to
//! *which* summary family/kind answers the intent, the same way
//! [`asap_types::post_asap::GroupingStrategy`]'s own doc explains.
//!
//! ## Placement: `SummaryExpr::SummaryAgg`, not `Implementation`/`SummaryFamilyType`
//!
//! Issue #256 as filed sketched this as a new field on
//! `Implementation::Sketch`/`Sample`/`Wavelet` ([`crate::replacement`])
//! and, correspondingly, on `SummaryFamilyType::Sketch`/`Sample`/`Wavelet`
//! ([`asap_types::post_asap`]) — a breaking change to those variants that
//! would ripple through every construction and match arm of `Implementation`
//! across `replacement.rs` and `cost_model.rs` (and every test in each),
//! because those two enums'
//! variants appear everywhere a summary family is discussed, whether or not
//! grouping is even in scope for the decision being made.
//!
//! This module places it on [`asap_types::post_asap::SummaryExpr::SummaryAgg`]
//! instead, for a reason visible directly in the code this axis has to gate
//! against: [`crate::replacement::implementations_for_with`] — the
//! function that actually produces an `Implementation` — takes only an
//! `&AggIntent`. It never sees a `Reduction`/`by` at all, because the
//! sketch-vs-exact boundary decision is genuinely independent of grouping.
//! Bolting a `GroupingStrategy` field onto `Implementation` would force every
//! caller of `implementations_for_with` to invent a `Reduction` it doesn't
//! have merely to populate a field the function's own logic never consults —
//! the false-orthogonality problem this axis is supposed to solve,
//! reintroduced one layer up.
//!
//! `SummaryAgg` already carries the one field this axis's legality actually
//! depends on: `reduction` (the `by` keys), right alongside `family` (the
//! `SummaryFamilyType` the "which kind" axis lives on). Placing
//! `GroupingStrategy` there means:
//!
//! - it touches exactly one producer — `replacement::construct_summary_agg`
//!   (module-private; reached through [`crate::replacement::construct_summary`])
//!   — instead of every match arm of `Implementation`/`SummaryFamilyType`
//!   across four modules;
//! - the legality check (non-empty `by`) is a local read of a field already
//!   in scope at the point the decision is made, not a value threaded in
//!   from a caller three layers up;
//! - existing `Implementation`/`SummaryFamilyType` match sites — including
//!   every deployment's own downstream code matching on either enum —
//!   observe zero change, because neither enum's shape changed at all.
//!
//! (Issue #256 has been updated via `gh issue comment` to record this
//! placement delta — see that issue for the note, mirroring how issue #251's
//! actual `Replacement` enum shape ended up slightly different from its own
//! original sketch.)
//!
//! ## Legality vs. cost (same split [`crate::replacement::implementations_for_with`]
//! already draws)
//!
//! This module only answers "is `SharedMultiSubpopulation` valid here at
//! all", never "is it worth it":
//!
//! - **Non-empty `by`** ([`has_subpopulations`]): an aggregate with no
//!   subpopulation concept (a global reduction, or a per-entity reduction
//!   with no grouping concept at all) has nothing for a
//!   shared-multi-subpopulation structure to multiplex across.
//! - **The family has a Hydra variant**
//!   ([`asap_types::post_asap::hydra_kind_for`]): only `SketchAlgorithm::Kll`
//!   has a modeled Hydra wrapper today (`HydraKll`) — see that function's own
//!   doc for why the others aren't candidates yet.
//!
//! Whether Hydra is *worth it* for a given estimated subpopulation
//! cardinality is a cost-model question, deliberately out of scope here.
//!
//! ## No `ForceSketchKind`-style steering — bind one already-known candidate directly
//!
//! An earlier draft of this module (written against the very first draft of
//! #251) reused a `CostModel`-wrapping adapter that "steered" a
//! whole-recursive-bind decision procedure toward a specific `SketchKind`,
//! the same pattern [`crate::replacement::SketchAlgorithmStrategy`]'s own module
//! docs explain was deliberately deleted from this crate as an anti-pattern:
//! forcing a choice via a whole-tree `CostModel` adapter had a real bug where
//! the forced choice could leak into a target's own nested aggregates. This
//! module never needs that: [`crate::replacement::implementations_for_with`]
//! already returns every ranked candidate `Implementation` directly, so
//! [`build_candidate`](HydraGroupingStrategy::build_candidate) just finds the
//! one whose `Implementation::Sketch(kind)` has `kind.algorithm()` matching
//! the Hydra-eligible `sketch_kind` it's building a candidate for, and
//! passes that exact,
//! already-decided `Implementation` to
//! [`crate::replacement::construct_summary`] — the same first-class,
//! one-candidate-at-a-time primitive [`crate::replacement::SketchAlgorithmStrategy`]
//! itself calls once per candidate. No adapter, no steering, no risk of a
//! forced choice leaking into nested aggregates.
//!
//! ## Cross-axis legality with roll-up (issue #254)
//!
//! Roll-up and Hydra currently operate on disjoint candidates. Roll-up
//! rewrites exact `Sum`/`Min`/`Max`/`Count` aggregates in the pre-ASAP DAG;
//! Hydra is offered only for approximate KLL quantiles and produces a
//! terminal post-ASAP summary candidate. Consequently neither strategy can
//! presently offer the other's candidate as a source. If roll-up support is
//! extended to mergeable sketches, that extension must consult
//! `rollup::is_legal_rollup_source` and add explicit Hydra merge semantics;
//! grouping alone must not imply that a sketch can be rolled up.

use std::rc::Rc;

use asap_types::post_asap::{
    default_hydra_params, hydra_kind_for, GroupingStrategy, HydraKind, SketchAlgorithm,
    SketchParams, SummaryExpr, SummaryFamilyType, SummaryNode,
};
use asap_types::pre_asap::agg_intent::AggIntent;
use asap_types::pre_asap::query_expr::{QueryExpr, Reduction};

use crate::cost_model::{CostModel, DefaultCostModel};
use crate::replacement::{
    bindable_intent, construct_summary, describe_intent, implementations_for_with,
    summary_candidates, Implementation, Replacement, ReplacementStrategy, ReplacementSubDAG,
    TargetSubDAG,
};

/// Whether `reduction` has a genuine subpopulation concept for
/// `GroupingStrategy::SharedMultiSubpopulation` to multiplex across — the
/// non-empty-`by` legality condition issue #256 requires.
///
/// - [`Reduction::PerEntity`]: no grouping concept at all (never merges
///   across entities) — `false`.
/// - [`Reduction::Reduce`] with an empty, non-`without` `by`: a genuine full
///   reduction, one output row, no subpopulations — `false`.
/// - [`Reduction::Reduce`] with a non-empty `by`, or any `without(...)`
///   exclusion grouping (which groups by whatever labels remain, even
///   `without([])` — "group by every label"): a real subpopulation concept
///   — `true`.
pub fn has_subpopulations(reduction: &Reduction) -> bool {
    match reduction.group_keys() {
        None => false,
        Some(keys) => keys.is_without() || !keys.is_empty(),
    }
}

/// A single static instance so [`HydraGroupingStrategy::default_cost_model`]
/// can hand out a `&'static dyn CostModel` without heap-allocating one — same
/// pattern [`crate::replacement::SketchAlgorithmStrategy`] uses.
static DEFAULT_COST_MODEL: DefaultCostModel = DefaultCostModel;

/// Wraps the `GroupingStrategy` axis (issue #256) as a
/// [`ReplacementStrategy`]: for a target [`SketchAlgorithmStrategy`](crate::replacement::SketchAlgorithmStrategy)
/// already has an opinion on, offers an additional
/// `GroupingStrategy::SharedMultiSubpopulation` candidate wherever the
/// legality conditions in the module docs above hold — alongside, not
/// instead of, the per-subpopulation candidates `SketchAlgorithmStrategy`
/// itself enumerates. The workload search composes both strategies over the
/// same target, so it sees every summary-family alternative *and* the Hydra
/// alternative; the built-in workload search registers both strategies, and
/// this strategy's own `replacements()` reports only the
/// latter, matching every other strategy in this crate's "one strategy, one
/// concern" shape.
pub struct HydraGroupingStrategy<'a> {
    cost_model: &'a dyn CostModel,
}

impl HydraGroupingStrategy<'static> {
    /// A strategy that ranks/binds via the built-in [`DefaultCostModel`] —
    /// what a deployment gets with no custom cost model plugged in, the same
    /// default [`crate::replacement::SketchAlgorithmStrategy::default_cost_model`]
    /// offers.
    pub fn default_cost_model() -> Self {
        Self {
            cost_model: &DEFAULT_COST_MODEL,
        }
    }
}

impl<'a> HydraGroupingStrategy<'a> {
    /// A strategy that ranks/binds via `cost_model` instead of the built-in
    /// static preference order — the same customization point
    /// [`crate::replacement::SketchAlgorithmStrategy::new`] already offers.
    pub fn new(cost_model: &'a dyn CostModel) -> Self {
        Self { cost_model }
    }

    /// Every legal `SharedMultiSubpopulation` candidate for `target` — empty
    /// when `target` isn't a bindable aggregate, has no subpopulation
    /// concept, or its intent's candidate summary families have no Hydra
    /// variant modeled.
    fn hydra_candidates(&self, target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG> {
        let QueryExpr::Aggregate { reduction, .. } = target.root.as_ref() else {
            return Vec::new();
        };
        if !has_subpopulations(reduction) {
            return Vec::new();
        }
        let Some(intent) = bindable_intent(target.root) else {
            return Vec::new();
        };
        summary_candidates(intent)
            .iter()
            .filter_map(|kind| hydra_kind_for(kind).map(|hydra_kind| (kind.clone(), hydra_kind)))
            .filter_map(|(sketch_kind, hydra_kind)| {
                self.build_candidate(target.root, intent, sketch_kind, hydra_kind)
            })
            .collect()
    }

    /// Find the already-ranked candidate [`Implementation::Sketch`] matching
    /// `sketch_kind` among [`implementations_for_with`]'s exhaustive list for
    /// `intent`, bind `root` to that exact, already-decided candidate via
    /// [`crate::replacement::construct_summary`] (no steering/forcing — see
    /// the module docs' "No `ForceSketchKind`-style steering"), then swap the
    /// resulting `SummaryAgg`'s `grouping` field from the default
    /// `PerSubpopulationInstance` to
    /// `SharedMultiSubpopulation { kind: hydra_kind, .. }` — reusing the
    /// entire bind decision procedure (schema derivation, column resolution,
    /// readout construction) unchanged, patching only the one field this
    /// axis owns.
    fn build_candidate(
        &self,
        root: &Rc<QueryExpr>,
        intent: &AggIntent,
        sketch_kind: SketchAlgorithm,
        hydra_kind: HydraKind,
    ) -> Option<ReplacementSubDAG> {
        let implementation = implementations_for_with(intent, self.cost_model)
            .into_iter()
            .find(|candidate| {
                matches!(candidate, Implementation::Sketch(kind) if *kind.algorithm() == sketch_kind)
            })?;
        let node = construct_summary(root, implementation, self.cost_model).ok()?;
        let k = per_subpopulation_k(&node)?;
        let params = default_hydra_params(hydra_kind.clone(), k);
        let grouping = GroupingStrategy::SharedMultiSubpopulation {
            kind: hydra_kind.clone(),
            params,
        };

        let patched = with_grouping(node, grouping);
        Some(ReplacementSubDAG {
            replacement: Replacement::Summary(patched),
            rationale: format!(
                "{} realizes as a shared {hydra_kind:?} structure over {sketch_kind:?} \
                 serving every subpopulation of this grouped aggregate, instead of one \
                 {sketch_kind:?} instance per distinct `by` key — legal because this \
                 aggregate has a non-empty subpopulation concept and {sketch_kind:?} has a \
                 modeled Hydra variant (asap_types::post_asap::hydra_kind_for); whether it's \
                 *worth* the shared/independent trade-off for the actual subpopulation \
                 cardinality is a CostModel's call, not this strategy's",
                describe_intent(intent)
            ),
        })
    }
}

impl ReplacementStrategy for HydraGroupingStrategy<'_> {
    fn matches(&self, target: &TargetSubDAG<'_>) -> bool {
        !self.hydra_candidates(target).is_empty()
    }

    fn replacements(&self, target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG> {
        self.hydra_candidates(target)
    }
}

/// The `k` a bound sketch candidate's `SummaryAgg` committed to, if its
/// family is `Sketch(SketchKind::new(Kll, SketchParams::Kll { k }))` — the only shape
/// [`default_hydra_params`] currently knows how to build a `HydraParams` for
/// (mirrors [`hydra_kind_for`]'s own "KLL only, for now" scope). `None` for
/// any other bound shape (an exact accumulator, a pass-through, or a
/// different sketch family) — never expected here in practice, since
/// `hydra_candidates` only calls this for a `sketch_kind` it already
/// confirmed has a `HydraKind` via `hydra_kind_for`, but degrading to "no
/// candidate" rather than panicking keeps this as conservative as the rest
/// of this module.
fn per_subpopulation_k(node: &SummaryNode) -> Option<u32> {
    match &node.expr {
        SummaryExpr::SummaryEstimate { summary_input, .. } => per_subpopulation_k(summary_input),
        SummaryExpr::SummaryAgg {
            family: SummaryFamilyType::Sketch(kind),
            ..
        } => match kind.params() {
            SketchParams::Kll { k } => Some(*k),
            _ => None,
        },
        _ => None,
    }
}

/// Rebuild `node`, replacing its `SummaryAgg`'s `grouping` field with
/// `grouping` — patching the one field this axis owns onto an
/// already-correctly-bound node rather than re-deriving the rest of it.
/// Recurses through a `SummaryEstimate` readout wrapper (the shape every
/// sketch candidate this module builds actually has) to reach the
/// `SummaryAgg` underneath.
fn with_grouping(node: Rc<SummaryNode>, grouping: GroupingStrategy) -> Rc<SummaryNode> {
    match &node.expr {
        SummaryExpr::SummaryEstimate {
            summary_input,
            query,
        } => Rc::new(SummaryNode {
            expr: SummaryExpr::SummaryEstimate {
                summary_input: with_grouping(Rc::clone(summary_input), grouping),
                query: query.clone(),
            },
            schema: node.schema.clone(),
        }),
        SummaryExpr::SummaryAgg {
            child,
            family,
            col,
            reduction,
            ..
        } => Rc::new(SummaryNode {
            expr: SummaryExpr::SummaryAgg {
                child: Rc::clone(child),
                family: family.clone(),
                col: col.clone(),
                reduction: reduction.clone(),
                grouping,
            },
            schema: node.schema.clone(),
        }),
        // Never reached by this module's own callers (they only ever pass a
        // node `construct_summary` just bound for a `Sketch`
        // candidate, which is always `SummaryAgg` or
        // `SummaryEstimate(SummaryAgg)`) — returning the node unchanged
        // rather than panicking keeps this as conservative as the rest of
        // the module if that ever stops holding.
        _ => node,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::post_asap::{HydraParams, SketchKind};
    use asap_types::pre_asap::agg_intent::{default_cardinality, default_quantile};
    use asap_types::pre_asap::query_expr::Source;
    use asap_types::pre_asap::schema::{Column, DataType, Schema};
    use asap_types::types::AccuracyTarget;

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

    fn agg_per_entity(intent: AggIntent, child: QueryExpr) -> QueryExpr {
        QueryExpr::Aggregate {
            reduction: Reduction::PerEntity,
            measures: vec![intent],
            output_names: vec![],
            having: None,
            child: Rc::new(child),
        }
    }

    // ── has_subpopulations ────────────────────────────────────────────────

    #[test]
    fn per_entity_has_no_subpopulation_concept() {
        assert!(!has_subpopulations(&Reduction::PerEntity));
    }

    #[test]
    fn empty_by_reduction_has_no_subpopulation_concept() {
        assert!(!has_subpopulations(&Reduction::by(vec![])));
    }

    #[test]
    fn non_empty_by_reduction_has_a_subpopulation_concept() {
        assert!(has_subpopulations(&Reduction::by(vec![2])));
    }

    #[test]
    fn without_grouping_has_a_subpopulation_concept_even_when_empty() {
        use asap_types::pre_asap::query_expr::GroupKeys;
        // `without([])` groups by every remaining label — a real
        // subpopulation concept, unlike `by([])`'s genuine full reduction.
        assert!(has_subpopulations(&Reduction::Reduce(GroupKeys::without(
            vec![]
        ))));
    }

    // ── HydraGroupingStrategy ─────────────────────────────────────────────

    #[test]
    fn matches_a_grouped_quantile_aggregate() {
        let q = Rc::new(agg(vec![2], default_quantile(0.99), metric_scan(&["job"])));
        let target = TargetSubDAG::new(&q);
        assert!(HydraGroupingStrategy::default_cost_model().matches(&target));
    }

    #[test]
    fn does_not_match_an_empty_by_aggregate() {
        // Global reduction — no subpopulation concept, no Hydra alternative.
        let q = Rc::new(agg(vec![], default_quantile(0.99), metric_scan(&["job"])));
        let target = TargetSubDAG::new(&q);
        let strategy = HydraGroupingStrategy::default_cost_model();
        assert!(!strategy.matches(&target));
        assert!(strategy.replacements(&target).is_empty());
    }

    #[test]
    fn does_not_match_a_per_entity_aggregate() {
        let q = Rc::new(agg_per_entity(
            default_quantile(0.99),
            metric_scan(&["job"]),
        ));
        let target = TargetSubDAG::new(&q);
        let strategy = HydraGroupingStrategy::default_cost_model();
        assert!(!strategy.matches(&target));
        assert!(strategy.replacements(&target).is_empty());
    }

    #[test]
    fn does_not_match_a_non_aggregate_node() {
        let scan = Rc::new(metric_scan(&["job"]));
        let target = TargetSubDAG::new(&scan);
        assert!(!HydraGroupingStrategy::default_cost_model().matches(&target));
    }

    #[test]
    fn quantile_offers_exactly_one_hydra_candidate_for_kll_only() {
        // summary_candidates(Quantile) = [Kll, DDSketch]; only Kll has a
        // modeled Hydra variant today, so exactly one candidate, not two.
        let q = Rc::new(agg(vec![2], default_quantile(0.99), metric_scan(&["job"])));
        let target = TargetSubDAG::new(&q);
        let replacements = HydraGroupingStrategy::default_cost_model().replacements(&target);
        assert_eq!(replacements.len(), 1, "{replacements:?}");

        let Replacement::Summary(node) = &replacements[0].replacement else {
            panic!("expected a Summary replacement");
        };
        let SummaryExpr::SummaryEstimate { summary_input, .. } = &node.expr else {
            panic!("expected SummaryEstimate root, got {:?}", node.expr);
        };
        let SummaryExpr::SummaryAgg {
            family,
            grouping,
            reduction,
            ..
        } = &summary_input.expr
        else {
            panic!("expected SummaryAgg, got {:?}", summary_input.expr);
        };
        assert_eq!(
            family,
            &SummaryFamilyType::Sketch(SketchKind::new(
                SketchAlgorithm::Kll,
                SketchParams::Kll { k: 200 }
            )),
            "the family/kind/params must be identical to the per-subpopulation candidate — \
             only `grouping` differs"
        );
        assert_eq!(reduction, &Reduction::by(vec![2]));
        assert_eq!(
            grouping,
            &GroupingStrategy::SharedMultiSubpopulation {
                kind: HydraKind::HydraKll,
                params: HydraParams::HydraKll {
                    k: 200,
                    shared_buckets: 200,
                },
            }
        );
        assert!(!replacements[0].rationale.is_empty());
    }

    #[test]
    fn cardinality_has_no_hydra_candidate_yet() {
        // summary_candidates(Cardinality) = [Hll, Theta, Kmv] — none have a
        // modeled Hydra variant, so no candidate at all (not an error, just
        // an empty result, same conservatism as every other strategy here).
        let q = Rc::new(agg(vec![2], default_cardinality(), metric_scan(&["job"])));
        let target = TargetSubDAG::new(&q);
        let strategy = HydraGroupingStrategy::default_cost_model();
        assert!(!strategy.matches(&target));
        assert!(strategy.replacements(&target).is_empty());
    }

    #[test]
    fn exact_accuracy_target_has_no_hydra_candidate() {
        // AccuracyTarget::Exact never binds a sketch at all — nothing for
        // this axis to offer a shared-structure alternative to.
        let intent = AggIntent::Quantile {
            col: None,
            q: 0.99,
            accuracy: AccuracyTarget::Exact,
        };
        let q = Rc::new(agg(vec![2], intent, metric_scan(&["job"])));
        let target = TargetSubDAG::new(&q);
        let strategy = HydraGroupingStrategy::default_cost_model();
        assert!(!strategy.matches(&target));
        assert!(strategy.replacements(&target).is_empty());
    }

    #[test]
    fn exact_mergeable_intent_has_no_hydra_candidate() {
        // Sum's exact accumulator has no candidate summary families at all
        // (summary_candidates only covers approximate-capable intents).
        let q = Rc::new(agg(
            vec![2],
            AggIntent::Sum { col: None },
            metric_scan(&["job"]),
        ));
        let target = TargetSubDAG::new(&q);
        let strategy = HydraGroupingStrategy::default_cost_model();
        assert!(!strategy.matches(&target));
        assert!(strategy.replacements(&target).is_empty());
    }

    #[test]
    fn does_not_match_a_multi_intent_or_having_aggregate() {
        let strategy = HydraGroupingStrategy::default_cost_model();

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
    }

    /// A custom `CostModel` doesn't change *which* candidate is offered —
    /// only which sketch candidate `implementations_for_with` itself would
    /// have ranked first, and how that candidate's own params are sized —
    /// same guarantee `SketchAlgorithmStrategy` makes for its own candidates.
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
    fn custom_cost_model_still_only_offers_the_kll_hydra_candidate() {
        let q = Rc::new(agg(vec![2], default_quantile(0.99), metric_scan(&["job"])));
        let target = TargetSubDAG::new(&q);
        let custom = PreferDDSketch;
        let replacements = HydraGroupingStrategy::new(&custom).replacements(&target);
        // DDSketch has no Hydra variant, so re-ranking DDSketch first at the
        // boundary doesn't add a second Hydra candidate or remove the Kll
        // one — `summary_candidates` (not `implementations_for_with`'s own
        // ranking) is what this strategy iterates.
        assert_eq!(replacements.len(), 1, "{replacements:?}");
    }
}
