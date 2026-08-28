//! `GroupingStrategy` (issue #256, part of #33): the axis deciding whether a
//! grouped aggregate's summary state is built as one independent instance
//! per `by` subpopulation (today's only, implicit behavior) or as one
//! shared Hydra-family structure serving all of them — orthogonal to
//! *which* summary family/kind answers the intent, the same way
//! [`asap_types::post_asap::GroupingStrategy`]'s own doc explains.
//!
//! ## Placement: planning metadata and edge-state type
//!
//! `SummaryExpr::SummaryAgg` carries the grouping choice next to the
//! `Reduction` whose `by` keys determine legality. The same choice is also
//! committed to `SummaryFamilyType::Sketch` on the aggregate's output edge.
//! That duplication is intentional: the node field makes the choice easy to
//! inspect during planning, while the edge type ensures an independent KLL/
//! CMS state and a Hydra-backed state cannot be accepted as compatible inputs
//! to a downstream `SummaryMerge`. [`with_grouping`] updates both atomically.
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
//!   ([`asap_types::post_asap::hydra_kind_for`]): `Cms` and `CountSketch`
//!   have structural Hydra mappings. `HydraKll` remains an explicit
//!   experimental IR value, but the paper excludes quantiles and search
//!   therefore never emits it. The shared-grid term is represented
//!   symbolically and accuracy-targeted candidates are withheld until its
//!   required statistics are supplied.
//!
//! Whether Hydra is *worth it* for a given estimated subpopulation
//! cardinality is a cost-model question, deliberately out of scope here —
//! candidates with no modeled error bound are excluded before costing.
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
//! Hydra is offered only for approximate quantile/count intents (KLL, CMS,
//! Count-Sketch) and produces a terminal post-ASAP summary candidate.
//! Consequently neither strategy can
//! presently offer the other's candidate as a source. If roll-up support is
//! extended to mergeable sketches, that extension must consult
//! `rollup::is_legal_rollup_source` and add explicit Hydra merge semantics;
//! grouping alone must not imply that a sketch can be rolled up.

use std::rc::Rc;

use asap_types::post_asap::{
    default_hydra_params, hydra_kind_for, BoundExpr, CompositionOperator, GroupingStrategy,
    GuaranteeSource, HydraKind, ProbabilityExpr, ResultGuarantee, SketchAlgorithm, SketchParams,
    SummaryExpr, SummaryFamilyType, SummaryNode,
};
use asap_types::pre_asap::agg_intent::AggIntent;
use asap_types::pre_asap::query_expr::{QueryExpr, Reduction};

use crate::cost_model::{CostModel, DefaultCostModel};
use crate::replacement::{
    accuracy_target, bindable_intent, construct_summary, describe_intent, implementations_for_with,
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
        // Hydra's shared-grid term is symbolic until deployment/data
        // statistics instantiate it. Keep that unevaluable guarantee out of
        // cost ranking when this node has an accuracy requirement.
        if accuracy_target(intent).is_some() {
            return Vec::new();
        }
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
        let per_subpopulation_params = per_subpopulation_sketch_params(&node)?;
        let params = default_hydra_params(hydra_kind.clone(), &per_subpopulation_params)?;
        let grouping = GroupingStrategy::SharedMultiSubpopulation {
            kind: hydra_kind.clone(),
            params,
        };

        let patched = with_grouping(node, grouping);
        Some(ReplacementSubDAG {
            strategy: "HydraGroupingStrategy",
            replacement: Replacement::Summary(patched),
            provenance: crate::replacement::ReplacementProvenance::SummaryImplementation,
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

/// The [`SketchParams`] a bound sketch candidate's `SummaryAgg` committed
/// to, if its family is `Sketch(_)` at all — `None` for any other bound
/// shape (an exact accumulator, a pass-through, or a family with no
/// `SketchParams`), never expected here in practice since `hydra_candidates`
/// only calls this for a `sketch_kind` it already confirmed has a
/// `HydraKind` via `hydra_kind_for`, but degrading to "no candidate" rather
/// than panicking keeps this as conservative as the rest of this module.
///
/// Deliberately returns the *whole* [`SketchParams`], not one scalar field
/// pulled out of it (an earlier version of this function assumed
/// `SketchParams::Kll { k }` specifically and returned a bare `k: u32`).
/// This axis is a "sketch of sketches" framework: the inner sketch a Hydra
/// structure wraps isn't always KLL, and each [`HydraKind`] variant needs
/// its own inner sketch's own knobs — `HydraCms`/`HydraCountSketch` need
/// (`width`, `depth`), not a `k`. [`default_hydra_params`] is what actually
/// destructures the right variant for `kind`; this function's only job is
/// to find whatever `SketchParams` the bind decision already committed to
/// and hand the whole thing over unchanged.
fn per_subpopulation_sketch_params(node: &SummaryNode) -> Option<SketchParams> {
    match &node.expr {
        SummaryExpr::SummaryEstimate { summary_input, .. } => {
            per_subpopulation_sketch_params(summary_input)
        }
        SummaryExpr::SummaryAgg {
            family: SummaryFamilyType::Sketch(kind, _),
            ..
        } => Some(kind.params().clone()),
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
            guarantee: node.guarantee.as_ref().map(hydra_guarantee),
        }),
        SummaryExpr::SummaryAgg {
            child,
            family,
            col,
            reduction,
            ..
        } => {
            let grouped_family = match family {
                SummaryFamilyType::Sketch(kind, _) => {
                    SummaryFamilyType::Sketch(kind.clone(), grouping.clone())
                }
                _ => family.clone(),
            };
            let mut grouped_schema = node.schema.clone();
            for field in &mut grouped_schema.fields {
                if let SummaryFamilyType::Sketch(kind, _) = &field.dtype {
                    field.dtype = SummaryFamilyType::Sketch(kind.clone(), grouping.clone());
                }
            }
            Rc::new(SummaryNode {
                expr: SummaryExpr::SummaryAgg {
                    child: Rc::clone(child),
                    family: grouped_family,
                    col: col.clone(),
                    reduction: reduction.clone(),
                    grouping,
                },
                schema: grouped_schema,
                guarantee: None,
            })
        }
        // Never reached by this module's own callers (they only ever pass a
        // node `construct_summary` just bound for a `Sketch`
        // candidate, which is always `SummaryAgg` or
        // `SummaryEstimate(SummaryAgg)`) — returning the node unchanged
        // rather than panicking keeps this as conservative as the rest of
        // the module if that ever stops holding.
        _ => node,
    }
}

/// Compose the inner per-subpopulation guarantee with Hydra's outer shared
/// grid. The paper's collision term depends on deployment/data statistics;
/// keeping those leaves symbolic makes the formula explicit while ensuring
/// target satisfaction fails closed until a caller supplies them.
fn hydra_guarantee(inner: &ResultGuarantee) -> ResultGuarantee {
    let mut provenance = inner.provenance.clone();
    provenance.push(GuaranteeSource::ChildGuarantee {
        input_index: 0,
        guarantee: Box::new(inner.clone()),
    });
    provenance.push(GuaranteeSource::UnavailableStatistic {
        statistic: "hydra_shared_grid_collision_bound".into(),
    });
    provenance.push(GuaranteeSource::UnavailableStatistic {
        statistic: "hydra_shared_grid_failure_probability".into(),
    });
    provenance.push(GuaranteeSource::CompositionStep {
        operator: CompositionOperator::ApproximateAggregate,
        rule: "hydra_shared_grid_union_bound".into(),
    });
    ResultGuarantee {
        metric: inner.metric,
        bound: BoundExpr::Sum {
            terms: vec![
                inner.bound.clone(),
                BoundExpr::Unknown {
                    statistic: "hydra_shared_grid_collision_bound".into(),
                },
            ],
        },
        failure_probability: ProbabilityExpr::UnionBound {
            terms: vec![
                inner.failure_probability.clone(),
                ProbabilityExpr::Unknown {
                    statistic: "hydra_shared_grid_failure_probability".into(),
                },
            ],
        },
        provenance,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::post_asap::ErrorMetric;
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

    #[test]
    fn hydra_composes_inner_and_shared_grid_error_symbolically() {
        let inner = ResultGuarantee {
            metric: ErrorMetric::Frequency,
            bound: BoundExpr::Constant { value: 0.01 },
            failure_probability: ProbabilityExpr::Constant { value: 0.02 },
            provenance: vec![],
        };
        let composed = hydra_guarantee(&inner);

        assert_eq!(composed.metric, ErrorMetric::Frequency);
        assert!(matches!(
            composed.bound,
            BoundExpr::Sum { ref terms }
                if matches!(terms.as_slice(), [
                    BoundExpr::Constant { value },
                    BoundExpr::Unknown { statistic },
                ] if *value == 0.01 && statistic == "hydra_shared_grid_collision_bound")
        ));
        assert!(matches!(
            composed.failure_probability,
            ProbabilityExpr::UnionBound { ref terms }
                if matches!(terms.as_slice(), [
                    ProbabilityExpr::Constant { value },
                    ProbabilityExpr::Unknown { statistic },
                ] if *value == 0.02 && statistic == "hydra_shared_grid_failure_probability")
        ));
    }

    // ── HydraGroupingStrategy ─────────────────────────────────────────────

    #[test]
    fn does_not_match_a_grouped_count_with_an_unprovable_accuracy_target() {
        let intent = AggIntent::Count {
            accuracy: AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.01,
            },
        };
        let q = Rc::new(agg(vec![2], intent, metric_scan(&["job"])));
        let target = TargetSubDAG::new(&q);
        assert!(!HydraGroupingStrategy::default_cost_model().matches(&target));
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
    fn quantile_has_no_hydra_candidate_without_a_modeled_error_bound() {
        let q = Rc::new(agg(vec![2], default_quantile(0.99), metric_scan(&["job"])));
        let target = TargetSubDAG::new(&q);
        let replacements = HydraGroupingStrategy::default_cost_model().replacements(&target);
        assert!(replacements.is_empty(), "{replacements:?}");
    }

    #[test]
    fn count_with_an_accuracy_target_has_no_hydra_candidate() {
        let intent = AggIntent::Count {
            accuracy: AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.01,
            },
        };
        let q = Rc::new(agg(vec![2], intent, metric_scan(&["job"])));
        let target = TargetSubDAG::new(&q);
        let replacements = HydraGroupingStrategy::default_cost_model().replacements(&target);
        assert!(replacements.is_empty(), "{replacements:?}");
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
    fn custom_cost_model_cannot_enable_unproven_hydra_kll() {
        let q = Rc::new(agg(vec![2], default_quantile(0.99), metric_scan(&["job"])));
        let target = TargetSubDAG::new(&q);
        let custom = PreferDDSketch;
        let replacements = HydraGroupingStrategy::new(&custom).replacements(&target);
        assert!(replacements.is_empty(), "{replacements:?}");
    }
}
