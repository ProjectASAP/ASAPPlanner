//! Cost model interface (issues #6, #33).
//!
//! `asap-plan` deliberately has no cost model *implementation* of its own —
//! ranking candidate summaries by real cost (bandwidth budget, memory
//! footprint, site count, observed drift, workload-level CSE credit, …)
//! needs knowledge this crate doesn't have and shouldn't acquire: the crate
//! doc's layering invariant is that `asap-plan` depends only on [`asap_ir`],
//! never on a runtime or a deployment model. What it *can* own is the
//! interface every deployment's cost model plugs into, so [`boundary`]'s
//! summary selection has exactly one extension point instead of forcing
//! each downstream (ASAPCollector + ASAPQuery-backend, ASAPFusion, …) to
//! fork [`boundary::implementation_for`].
//!
//! This trait is scoped to the approximate-**sketch** family specifically
//! ([`CostModel::rank_candidates`]/[`size_params`](CostModel::size_params)
//! take/return [`SketchKind`]/[`SketchParams`]) — `asap_sketch` also has
//! sibling families for sampling-based, wavelet-transform, and fitted
//! statistical-model summaries
//! ([`asap_types::post_asap::SamplingKind`]/…/[`asap_types::post_asap::StatModelKind`]),
//! each with its own `(Kind, Params)` pair, deliberately *not* folded into
//! this trait: no core `AggIntent` picks one of those families today (only
//! [`CostModel::realize_extension`] can, for a deployment-specific
//! `AggIntent::Extension`), so there is no ranking/sizing decision for this
//! trait to own yet. Should a family other than `Sketch` ever need its own
//! `rank_candidates`/`size_params`, it gets its own trait methods rather
//! than overloading these ones across incompatible `Kind`/`Params` types.
//!
//! Every entry point that doesn't take an explicit `&dyn CostModel`
//! ([`implementation_for`](crate::boundary::implementation_for),
//! [`implement_tree`](crate::bind::implement_tree)) runs against
//! [`DefaultCostModel`], so a deployment that never plugs in its own cost
//! model keeps today's static-preference-order behavior exactly, byte for
//! byte.
//!
//! ## CSE sharing (issue #237, #223 stage 4)
//!
//! [`CseCandidate`]/[`ShareDecision`]/[`CostModel::cse_share_decision`] below
//! decide whether a CSE-detected shared subtree
//! ([`asap_types::pre_asap::cse::share_common_subtrees`], issue #223 stages
//! 1-2, PR #235) is actually worth sharing, via a real Volcano/Cascades-style
//! cost comparison rather than a fixed rule. See
//! `docs/cse-cost-model-decision.md` for the full design discussion (why
//! cost-based, why not a full plan-search engine, the layering constraint
//! that forces detection to stay cost-agnostic). [`bind::implement_workload_with`](crate::bind::implement_workload_with)
//! is the caller.

use asap_types::post_asap::{
    SketchKind, SketchParams, SketchQuery, SummaryFamilyType, SummaryNode,
};
use asap_types::pre_asap::agg_intent::AggIntent;
use asap_types::pre_asap::expr_ir::ColumnRef;
use asap_types::pre_asap::query_expr::QueryExpr;

use crate::boundary::Implementation;

/// A CSE-detected, legality-gated shared subtree with two or more consumers
/// — the unit [`CostModel::cse_share_decision`] decides over. Built by
/// [`bind::implement_workload_with`](crate::bind::implement_workload_with)
/// the first time it binds a subtree that
/// [`asap_types::pre_asap::cse::share_common_subtrees`] already collapsed
/// onto one `Rc` for two or more workload roots. See
/// `docs/cse-cost-model-decision.md`.
pub struct CseCandidate<'a> {
    /// The shared pre-ASAP subtree itself.
    pub subtree: &'a QueryExpr,
    /// The `SummaryNode` this subtree bound to — gives the cost model the
    /// concrete `SummaryFamilyType`/`(kind, params)` actually at stake, not
    /// just the pre-ASAP shape.
    pub bound_summary: &'a SummaryNode,
    /// How many workload roots reference this exact shared subtree, counted
    /// once up front over the whole workload (always >= 2 — a candidate is
    /// only ever constructed for an actually-shared subtree).
    pub consumer_count: usize,
}

/// The decision [`CostModel::cse_share_decision`] returns for one
/// [`CseCandidate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareDecision {
    /// Reuse one bound `SummaryNode` across every consumer.
    Share,
    /// Bind each occurrence independently — the shared-maintenance cost
    /// isn't worth it for this candidate.
    RecomputeIndependently,
}

/// Default [`CostModel::cse_recompute_cost`]: a structural-size proxy — the
/// number of *unique* nodes in `subtree`'s DAG
/// ([`asap_types::pre_asap::cse::dag_node_count`], the same module this
/// candidate's sharing was detected in). Deliberately **not** a raw
/// `serde_json` serialization length: after CSE, `subtree` is generally a
/// DAG, not a tree (a `CseCandidate` only exists because something got
/// shared), and a naive full serialization re-serializes — over-counts —
/// any descendant `subtree` already shares internally, once per parent
/// that references it, instead of once for the whole DAG. `dag_node_count`
/// dedupes by `Rc` pointer identity, so it charges each unique node's
/// contribution exactly once regardless of how many places within
/// `subtree` reference it. Cheap to compute (one pass, no serialization),
/// and still scales with real structural complexity — a genuinely tiny
/// leaf costs little to recompute, a deep multi-join subtree costs a lot.
/// A deployment with real per-row/per-update cost knowledge should
/// override [`CostModel::cse_recompute_cost`] instead of relying on this.
pub fn default_cse_recompute_cost(subtree: &QueryExpr) -> f64 {
    asap_types::pre_asap::cse::dag_node_count(subtree) as f64
}

/// Default [`CostModel::cse_shared_maintenance_cost`]: a small
/// per-[`SummaryFamilyType`] weight, scaled to the same order of magnitude
/// as [`default_cse_recompute_cost`]'s typical output (a small node
/// count, not a byte length), reflecting that families differ in how
/// expensive they are to keep *continuously updated* for the life of a
/// workload — an exact accumulator is the cheapest (an O(1) merge),
/// sketches/samples cost more (a whole data structure to update per new
/// row), wavelets/fitted models cost the most (coefficient/parameter
/// maintenance). These weights are illustrative, not measured — a
/// deployment with real memory/update-cost numbers should override
/// [`CostModel::cse_shared_maintenance_cost`] instead of relying on this
/// table.
pub fn default_cse_shared_maintenance_cost(family: &SummaryFamilyType) -> f64 {
    const UNIT: f64 = 1.0;
    let weight = match family {
        SummaryFamilyType::Plain(_) => 1.0,
        SummaryFamilyType::ExactAggregate(..) => 1.0,
        SummaryFamilyType::Sketch(..) => 3.0,
        SummaryFamilyType::Sample(..) => 3.0,
        SummaryFamilyType::Wavelet(..) => 5.0,
        SummaryFamilyType::StatModel(..) => 6.0,
    };
    weight * UNIT
}

/// Ranks the candidate summary families for one [`AggIntent`], best choice
/// first.
///
/// [`boundary::summary_candidates`] returns every family that *can* answer an
/// intent, in an arbitrary static preference order (issue #98's "one home"
/// for the candidate set). A `CostModel` re-orders that list under real,
/// deployment-specific cost knowledge this crate has no way to know about —
/// [`boundary::implementation_for_with`] implements whichever candidate ends
/// up first after ranking.
pub trait CostModel {
    /// Rank `candidates` (as returned by
    /// [`summary_candidates`](crate::boundary::summary_candidates)) for
    /// `intent`, best choice first.
    ///
    /// Implementations MAY reorder freely and MAY drop entries that aren't
    /// available in their deployment, but MUST NOT invent a candidate that
    /// wasn't in the input — an unknown [`SketchKind`] has no
    /// [`SketchParams`](asap_types::post_asap::SketchParams) sizing logic in
    /// [`boundary::implementation_for_with`] and binding it will panic.
    /// Returning an empty `Vec` means "no candidate is acceptable";
    /// `implementation_for_with` treats that the same as `candidates`
    /// having been empty to begin with.
    fn rank_candidates(&self, intent: &AggIntent, candidates: &[SketchKind]) -> Vec<SketchKind>;

    /// Size [`SketchParams`] for `kind` (one of the candidates
    /// [`rank_candidates`](Self::rank_candidates) put first) under the
    /// resolved `(eps, delta)` accuracy budget.
    ///
    /// Splitting sizing out from candidate selection lets a deployment own
    /// its own parameter-sizing math (e.g. an empirically-tuned table, or
    /// discrete rungs required by a downstream catalog) without forking
    /// [`boundary::implementation_for_with`] — the same "one extension
    /// point" rationale as `rank_candidates`, one level deeper. Default:
    /// [`boundary::default_size_params`], `asap-plan`'s built-in formulas
    /// (unchanged) — a deployment that only needs to reorder candidates,
    /// not resize them, can leave this method unimplemented.
    fn size_params(
        &self,
        kind: SketchKind,
        intent: &AggIntent,
        eps: f64,
        delta: f64,
    ) -> SketchParams {
        crate::boundary::default_size_params(kind, intent, eps, delta)
    }

    /// Realize an `AggIntent::Extension { ext_kind, payload }` — a
    /// deployment-specific intent shape core has no realization opinion
    /// for (issue #131). `boundary::implementation_for_with` consults this
    /// for every `Extension` node instead of hardcoding `PassThrough`
    /// (issue #150). Default: `PassThrough` — preserves today's behavior
    /// for every deployment that doesn't override this, exactly like
    /// `size_params`'s default-delegates pattern above.
    fn realize_extension(&self, _ext_kind: &str, _payload: &serde_json::Value) -> Implementation {
        Implementation::PassThrough
    }

    /// Build the `SummaryEstimate` readout for an `Extension` intent this
    /// same `CostModel` realized as `Implementation::Sketch` via
    /// [`realize_extension`](Self::realize_extension). Only ever called
    /// when `realize_extension` returned `Sketch` for the same
    /// `(ext_kind, payload)` — `bind::readout` has no other way to build a
    /// `SketchQuery` for a shape core doesn't know. A deployment that
    /// overrides `realize_extension` to return `Sketch` for some
    /// `ext_kind` MUST also override this for that same `ext_kind`, or
    /// this default panics loudly (rather than silently misinterpreting
    /// `payload`) the first time that intent is actually read out.
    fn readout_extension(
        &self,
        ext_kind: &str,
        _payload: &serde_json::Value,
        _col: &ColumnRef,
    ) -> SketchQuery {
        unimplemented!(
            "CostModel::realize_extension returned Sketch for ext_kind={ext_kind:?} but \
             readout_extension wasn't overridden to match"
        )
    }

    /// Estimate the one-time cost of recomputing `candidate.subtree`
    /// independently at a single use site. Default:
    /// [`default_cse_recompute_cost`] (a structural-size proxy). See
    /// `docs/cse-cost-model-decision.md`.
    fn cse_recompute_cost(&self, candidate: &CseCandidate) -> f64 {
        default_cse_recompute_cost(candidate.subtree)
    }

    /// Estimate the cost of maintaining `candidate.bound_summary` as one
    /// continuously-updated shared summary for the life of the workload.
    /// Default: [`default_cse_shared_maintenance_cost`] (a per-family
    /// weight table), applied to whichever field of
    /// `candidate.bound_summary`'s output schema actually carries summary
    /// state (falls back to the cheapest, `Plain`, weight if none does —
    /// e.g. `bound_summary` is a passthrough `Logical` node with nothing
    /// summary-shaped to maintain). See `docs/cse-cost-model-decision.md`.
    fn cse_shared_maintenance_cost(&self, candidate: &CseCandidate) -> f64 {
        let family = candidate
            .bound_summary
            .schema
            .fields
            .iter()
            .map(|f| &f.dtype)
            .find(|dtype| !matches!(dtype, SummaryFamilyType::Plain(_)))
            .cloned()
            .unwrap_or(SummaryFamilyType::Plain(
                asap_types::pre_asap::DataType::Float64,
            ));
        default_cse_shared_maintenance_cost(&family)
    }

    /// Decide whether to reuse one shared `SummaryNode` across every
    /// consumer of `candidate`, or bind each occurrence independently — a
    /// Volcano/Cascades-style cost comparison (issue #237, #223 stage 4; see
    /// `docs/cse-cost-model-decision.md`): share iff the estimated cost of
    /// maintaining one shared summary is no greater than the estimated total
    /// cost of recomputing it independently everywhere it's used.
    ///
    /// The default body composes [`cse_recompute_cost`](Self::cse_recompute_cost)
    /// and [`cse_shared_maintenance_cost`](Self::cse_shared_maintenance_cost)
    /// — a deployment with real cost knowledge should override those two
    /// (keeping this comparison), or override this method directly for a
    /// wholly different policy.
    fn cse_share_decision(&self, candidate: &CseCandidate) -> ShareDecision {
        let recompute_total = self.cse_recompute_cost(candidate) * candidate.consumer_count as f64;
        let shared = self.cse_shared_maintenance_cost(candidate);
        if shared <= recompute_total {
            ShareDecision::Share
        } else {
            ShareDecision::RecomputeIndependently
        }
    }
}

/// The default cost model: preserves [`summary_candidates`]'s built-in static
/// order and [`boundary::default_size_params`]'s built-in sizing unchanged.
///
/// [`summary_candidates`]: crate::boundary::summary_candidates
pub struct DefaultCostModel;

impl CostModel for DefaultCostModel {
    fn rank_candidates(&self, _intent: &AggIntent, candidates: &[SketchKind]) -> Vec<SketchKind> {
        candidates.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boundary::summary_candidates;
    use asap_types::pre_asap::agg_intent::default_cardinality;

    #[test]
    fn default_cost_model_preserves_static_order() {
        let intent = default_cardinality();
        let candidates = summary_candidates(&intent);
        assert_eq!(
            DefaultCostModel.rank_candidates(&intent, candidates),
            candidates.to_vec()
        );
    }

    struct AlwaysPreferLast;

    impl CostModel for AlwaysPreferLast {
        fn rank_candidates(
            &self,
            _intent: &AggIntent,
            candidates: &[SketchKind],
        ) -> Vec<SketchKind> {
            let mut v = candidates.to_vec();
            v.reverse();
            v
        }
    }

    #[test]
    fn custom_cost_model_can_reorder_candidates() {
        let intent = default_cardinality();
        let candidates = summary_candidates(&intent);
        let ranked = AlwaysPreferLast.rank_candidates(&intent, candidates);
        assert_eq!(ranked.first(), candidates.last());
    }

    /// A deployment that only overrides `rank_candidates` keeps
    /// `asap-plan`'s built-in sizing via the trait's default `size_params`
    /// body — the split is opt-in per method, not all-or-nothing.
    #[test]
    fn size_params_default_body_matches_default_size_params() {
        let intent = default_cardinality();
        assert_eq!(
            AlwaysPreferLast.size_params(SketchKind::Hll, &intent, 0.01, 0.01),
            crate::boundary::default_size_params(SketchKind::Hll, &intent, 0.01, 0.01),
        );
    }

    /// A deployment CAN override `size_params` independently of
    /// `rank_candidates` — e.g. to size against a catalog-constrained set
    /// of discrete parameter rungs instead of `asap-plan`'s continuous
    /// formulas.
    struct DiscreteKllRungs;

    impl CostModel for DiscreteKllRungs {
        fn rank_candidates(
            &self,
            _intent: &AggIntent,
            candidates: &[SketchKind],
        ) -> Vec<SketchKind> {
            candidates.to_vec()
        }

        fn size_params(
            &self,
            kind: SketchKind,
            intent: &AggIntent,
            eps: f64,
            delta: f64,
        ) -> SketchParams {
            match kind {
                SketchKind::Kll => {
                    let k = if eps >= 0.01 { 200 } else { 2048 };
                    SketchParams::Kll { k }
                }
                other => crate::boundary::default_size_params(other, intent, eps, delta),
            }
        }
    }

    #[test]
    fn custom_cost_model_can_override_sizing_independently_of_ranking() {
        use asap_types::pre_asap::agg_intent::default_quantile;

        let intent = default_quantile(0.99);
        assert_eq!(
            DiscreteKllRungs.size_params(SketchKind::Kll, &intent, 0.001, 0.01),
            SketchParams::Kll { k: 2048 },
        );
        // Untouched kinds still fall through to the default formula.
        assert_eq!(
            DiscreteKllRungs.size_params(SketchKind::Hll, &intent, 0.01, 0.01),
            crate::boundary::default_size_params(SketchKind::Hll, &intent, 0.01, 0.01),
        );
    }

    // ── CSE sharing (issue #237, #223 stage 4) ──────────────────────────

    use asap_types::post_asap::{ExactKind, ExactParams, SummaryExpr, SummaryField, SummarySchema};
    use asap_types::pre_asap::query_expr::Source;
    use asap_types::pre_asap::schema::{Column, DataType, Schema};

    fn scan() -> QueryExpr {
        QueryExpr::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Column::new("ts", DataType::Timestamp, false),
                    Column::new("value", DataType::Float64, false),
                ],
                0,
                vec![],
            ),
        }
    }

    fn summary_node(family: SummaryFamilyType) -> SummaryNode {
        SummaryNode {
            expr: SummaryExpr::SummaryAgg {
                child: std::rc::Rc::new(SummaryNode {
                    expr: SummaryExpr::Logical(Box::new(scan())),
                    schema: SummarySchema {
                        fields: vec![],
                        time_index: None,
                    },
                }),
                family: family.clone(),
                col: asap_types::pre_asap::expr_ir::ColumnRef::Named("value".into()),
                reduction: asap_types::pre_asap::query_expr::Reduction::by(vec![]),
            },
            schema: SummarySchema {
                fields: vec![SummaryField {
                    name: "state".into(),
                    dtype: family,
                    nullable: false,
                }],
                time_index: None,
            },
        }
    }

    #[test]
    fn default_recompute_cost_is_positive_and_grows_with_structural_size() {
        let leaf = scan();
        let nested = QueryExpr::Dedup {
            cols: vec![0],
            child: std::rc::Rc::new(leaf.clone()),
        };
        assert!(default_cse_recompute_cost(&leaf) > 0.0);
        assert!(default_cse_recompute_cost(&nested) > default_cse_recompute_cost(&leaf));
    }

    /// The DAG-awareness this proxy exists for: a subtree that internally
    /// re-references one shared descendant (e.g. after single-query CSE,
    /// `x op x` collapsing both branches onto one `Rc`) must cost the same
    /// as if that descendant only appeared once — not double, the way a
    /// naive tree-shaped size measure (a full serialization, or an
    /// identity-blind recursive walk) would count it.
    #[test]
    fn default_recompute_cost_does_not_double_count_an_internally_shared_descendant() {
        use asap_types::pre_asap::expr_ir::ScalarValue;
        use asap_types::pre_asap::query_expr::{JoinKind, Predicate};

        let true_pred = || {
            Predicate(std::rc::Rc::new(QueryExpr::Literal(ScalarValue::Boolean(
                true,
            ))))
        };
        let shared_leaf = std::rc::Rc::new(scan());
        let no_sharing = QueryExpr::Join {
            kind: JoinKind::Inner,
            pred: true_pred(),
            left: std::rc::Rc::new(scan()),
            right: std::rc::Rc::new(scan()),
        };
        let with_sharing = QueryExpr::Join {
            kind: JoinKind::Inner,
            pred: true_pred(),
            left: std::rc::Rc::clone(&shared_leaf),
            right: std::rc::Rc::clone(&shared_leaf),
        };
        assert_eq!(
            default_cse_recompute_cost(&no_sharing),
            3.0,
            "no sharing: Join + 2 independent Scans = 3 unique nodes"
        );
        assert_eq!(
            default_cse_recompute_cost(&with_sharing),
            2.0,
            "internal sharing: Join + 1 shared Scan (referenced twice) = \
             2 unique nodes, not 3 — a tree-shaped size measure would \
             wrongly charge for the shared Scan twice"
        );
    }

    #[test]
    fn default_shared_maintenance_cost_orders_families_cheapest_to_priciest() {
        let exact = default_cse_shared_maintenance_cost(&SummaryFamilyType::ExactAggregate(
            ExactKind::Sum,
            ExactParams::Sum,
        ));
        let sketch = default_cse_shared_maintenance_cost(&SummaryFamilyType::Sketch(
            SketchKind::Hll,
            SketchParams::Hll { precision: 12 },
        ));
        assert!(
            exact < sketch,
            "an exact accumulator should be cheaper to keep continuously updated \
             than a sketch: exact={exact}, sketch={sketch}"
        );
    }

    #[test]
    fn cse_share_decision_shares_when_recompute_dominates_maintenance() {
        let candidate = CseCandidate {
            subtree: &scan(),
            bound_summary: &summary_node(SummaryFamilyType::ExactAggregate(
                ExactKind::Sum,
                ExactParams::Sum,
            )),
            // Many consumers of a cheap accumulator: recompute_total should
            // dominate the fixed maintenance cost.
            consumer_count: 1000,
        };
        assert_eq!(
            DefaultCostModel.cse_share_decision(&candidate),
            ShareDecision::Share
        );
    }

    #[test]
    fn cse_share_decision_recomputes_when_maintenance_dominates_recompute() {
        let candidate = CseCandidate {
            subtree: &scan(),
            bound_summary: &summary_node(SummaryFamilyType::StatModel(
                asap_types::post_asap::StatModelKind::Parametric,
                asap_types::post_asap::StatModelParams::Parametric {
                    family: "gaussian_mixture".into(),
                },
            )),
            // A single, cheap-to-recompute leaf (scan() alone is 1 DAG
            // node, recompute_total = 1) against an expensive-to-maintain
            // family (StatModel, maintenance cost 6.0): maintenance should
            // dominate.
            consumer_count: 1,
        };
        assert_eq!(
            DefaultCostModel.cse_share_decision(&candidate),
            ShareDecision::RecomputeIndependently
        );
    }

    #[test]
    fn cse_share_decision_default_body_composes_the_two_cost_hooks() {
        struct AlwaysExpensiveToRecompute;
        impl CostModel for AlwaysExpensiveToRecompute {
            fn rank_candidates(
                &self,
                _intent: &AggIntent,
                candidates: &[SketchKind],
            ) -> Vec<SketchKind> {
                candidates.to_vec()
            }
            fn cse_recompute_cost(&self, _candidate: &CseCandidate) -> f64 {
                1e9
            }
        }

        // Even the priciest family should lose to an overridden recompute
        // cost this large, confirming `cse_share_decision`'s default body
        // actually calls through to the overridable hooks rather than
        // hardcoding a comparison against its own defaults.
        let candidate = CseCandidate {
            subtree: &scan(),
            bound_summary: &summary_node(SummaryFamilyType::StatModel(
                asap_types::post_asap::StatModelKind::Parametric,
                asap_types::post_asap::StatModelParams::Parametric {
                    family: "gaussian_mixture".into(),
                },
            )),
            consumer_count: 2,
        };
        assert_eq!(
            AlwaysExpensiveToRecompute.cse_share_decision(&candidate),
            ShareDecision::Share
        );
    }
}
