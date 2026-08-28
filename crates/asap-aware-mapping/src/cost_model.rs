//! Cost model interface (issues #6, #33).
//!
//! `asap-plan` deliberately has no cost model *implementation* of its own —
//! ranking candidate summaries by real cost (bandwidth budget, memory
//! footprint, site count, observed drift, workload-level CSE credit, …)
//! needs knowledge this crate doesn't have and shouldn't acquire: the crate
//! doc's layering invariant is that `asap-plan` depends only on [`asap_ir`],
//! never on a runtime or a deployment model. What it *can* own is the
//! interface every deployment's cost model plugs into, so [`replacement`]'s
//! summary selection has exactly one extension point instead of forcing
//! each downstream (ASAPCollector + ASAPQuery-backend, ASAPFusion, …) to
//! fork `replacement::implementations_for_with`.
//!
//! This trait is scoped to the approximate-**sketch** family specifically
//! ([`CostModel::rank_candidates`]/[`size_params`](CostModel::size_params)
//! take/return [`SketchAlgorithm`]/[`SketchParams`]) — `asap_sketch` also has
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
//! ([`SketchAlgorithmStrategy::default_cost_model`](crate::replacement::SketchAlgorithmStrategy::default_cost_model),
//! [`search_workload`](crate::replacement::search_workload)) runs against
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
//! `docs/design_docs/cse-cost-model-decision.md` for the full design discussion (why
//! cost-based, why not a full plan-search engine, the layering constraint
//! that forces detection to stay cost-agnostic).
//! [`PlanSpace::cost_sorted`](crate::replacement::PlanSpace::cost_sorted)
//! (via [`crate::replacement`]'s own `cse_preference`) and
//! [`DefaultCostModel::estimate_cost`] are this crate's own callers.

use std::rc::Rc;

use asap_types::post_asap::{
    GroupingStrategy, HydraParams, SketchAlgorithm, SketchParams, SketchQuery, SummaryExpr,
    SummaryFamilyType, SummaryNode,
};
use asap_types::pre_asap::agg_intent::AggIntent;
use asap_types::pre_asap::expr_ir::ColumnRef;
use asap_types::pre_asap::query_expr::QueryExpr;

use crate::recurrence::{
    self, Horizon, RecurrenceCostExplanation, RecurrenceError, RecurrenceProfile,
};
use crate::replacement::{
    realize_child, Implementation, Replacement, ReplacementProvenance, ReplacementSubDAG,
    TargetSubDAG,
};

/// A CSE-detected, legality-gated shared subtree with two or more consumers
/// — the unit [`CostModel::cse_share_decision`] decides over. Built by
/// [`PlanSpace::cost_sorted`](crate::replacement::PlanSpace::cost_sorted)
/// (via [`crate::replacement`]'s own `cse_preference`) the first time it
/// needs a representative bound node for a subtree that
/// [`asap_types::pre_asap::cse::share_common_subtrees`] already collapsed
/// onto one `Rc` for two or more workload roots. See
/// `docs/design_docs/cse-cost-model-decision.md`.
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

/// A cost estimate produced by a [`CostModel`] hook. A newtype around `f64`
/// rather than a bare `f64` return type, so a future cost dimension (e.g.
/// separate CPU/memory/network estimates, once a deployment actually needs
/// to compare along more than one axis) can be added as a field here
/// without changing every hook's signature a second time. Today it's still
/// a single unitless scalar — the same magnitude convention
/// [`default_cse_recompute_cost`]/[`default_cse_shared_maintenance_cost`]
/// already used as bare `f64`s, just wrapped.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Cost(pub f64);

impl Cost {
    /// The cost of an operation that costs nothing at all.
    pub const ZERO: Cost = Cost(0.0);
}

impl std::fmt::Display for Cost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::ops::Add for Cost {
    type Output = Cost;
    fn add(self, rhs: Cost) -> Cost {
        Cost(self.0 + rhs.0)
    }
}

impl std::ops::Mul<usize> for Cost {
    type Output = Cost;
    fn mul(self, rhs: usize) -> Cost {
        Cost(self.0 * rhs as f64)
    }
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
pub fn default_cse_recompute_cost(subtree: &QueryExpr) -> Cost {
    Cost(asap_types::pre_asap::cse::dag_node_count(subtree) as f64)
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
pub fn default_cse_shared_maintenance_cost(family: &SummaryFamilyType) -> Cost {
    const UNIT: f64 = 1.0;
    let weight = match family {
        SummaryFamilyType::Plain(_) => 1.0,
        SummaryFamilyType::ExactAggregate(..) => 1.0,
        SummaryFamilyType::Sketch(..) => 3.0,
        SummaryFamilyType::Sample(..) => 3.0,
        SummaryFamilyType::Wavelet(..) => 5.0,
        SummaryFamilyType::StatModel(..) => 6.0,
    };
    Cost(weight * UNIT)
}

/// Ranks the candidate sketch algorithms for one [`AggIntent`], best choice
/// first.
///
/// [`replacement::summary_candidates`] returns every algorithm that *can* answer an
/// intent, in an arbitrary static preference order (issue #98's "one home"
/// for the candidate set). A `CostModel` re-orders that list under real,
/// deployment-specific cost knowledge this crate has no way to know about —
/// `replacement::implementations_for_with` constructs every candidate in the
/// resulting order.
pub trait CostModel {
    /// Rank `candidates` (as returned by
    /// [`summary_candidates`](crate::replacement::summary_candidates)) for
    /// `intent`, best choice first.
    ///
    /// Implementations MAY reorder freely, but MUST return exactly the input
    /// candidates: no additions, removals, or duplicates. Candidate legality
    /// and availability belong to replacement generation, not costing; letting
    /// this hook filter would violate [`ReplacementStrategy`]'s exhaustive,
    /// never-prune contract. This invariant is checked at every production call
    /// site, and a violation panics with a contract error.
    ///
    /// [`ReplacementStrategy`]: crate::replacement::ReplacementStrategy
    fn rank_candidates(
        &self,
        intent: &AggIntent,
        candidates: &[SketchAlgorithm],
    ) -> Vec<SketchAlgorithm>;

    /// Size [`SketchParams`] for `kind` (one of the candidates
    /// [`rank_candidates`](Self::rank_candidates) put first) under the
    /// resolved `(eps, delta)` accuracy budget.
    ///
    /// Splitting sizing out from candidate selection lets a deployment own
    /// its own parameter-sizing math (e.g. an empirically-tuned table, or
    /// discrete rungs required by a downstream catalog) without forking
    /// `replacement::implementations_for_with` — the same "one extension
    /// point" rationale as `rank_candidates`, one level deeper. Default:
    /// [`replacement::default_size_params`], `asap-plan`'s built-in formulas
    /// (unchanged) — a deployment that only needs to reorder candidates,
    /// not resize them, can leave this method unimplemented.
    ///
    /// # Contract
    ///
    /// The returned parameters MUST make `kind` satisfy the supplied
    /// `(eps, delta)` accuracy budget. This is a semantic requirement, not a
    /// requirement that parameter fields themselves be numerically monotonic:
    /// catalog rungs and empirically tuned layouts are allowed, but returning
    /// a configuration that misses the requested budget makes the resulting
    /// plan invalid. Accuracy reconciliation relies on this same contract;
    /// any implementation satisfying a tighter budget necessarily satisfies
    /// a looser budget for the identical aggregate query.
    fn size_params(
        &self,
        kind: SketchAlgorithm,
        intent: &AggIntent,
        eps: f64,
        delta: f64,
    ) -> SketchParams {
        crate::replacement::default_size_params(kind, intent, eps, delta)
    }

    /// Estimated number of distinct subpopulations produced by `target`'s
    /// grouping keys. `None` means the deployment has no cardinality estimate;
    /// grouping alternatives remain legal but keep their discovery order.
    fn estimated_subpopulation_count(&self, _target: &QueryExpr) -> Option<usize> {
        None
    }

    /// Comparable memory-state cost for a sketch grouping candidate. The
    /// default compares `N` independent inner sketches against the complete
    /// shared Hydra grid, using [`Self::estimated_subpopulation_count`].
    fn grouping_state_cost(
        &self,
        candidate: &ReplacementSubDAG,
        target: &TargetSubDAG<'_>,
    ) -> Option<Cost> {
        let Replacement::Summary(node) = &candidate.replacement else {
            return None;
        };
        let (kind, grouping) = sketch_state(node)?;
        let inner = sketch_state_units(kind.params());
        let units = match grouping {
            GroupingStrategy::PerSubpopulationInstance => {
                inner * self.estimated_subpopulation_count(target.root)? as f64
            }
            GroupingStrategy::SharedMultiSubpopulation { params, .. } => {
                inner * hydra_grid_cells(params)
            }
        };
        Some(Cost(units))
    }

    /// Realize an `AggIntent::Extension { ext_kind, payload }` — a
    /// deployment-specific intent shape core has no realization opinion
    /// for (issue #131). `replacement::implementations_for_with` consults this
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
    /// `(ext_kind, payload)` — `replacement::readout` has no other way to build a
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
    /// `docs/design_docs/cse-cost-model-decision.md`.
    fn cse_recompute_cost(&self, candidate: &CseCandidate) -> Cost {
        default_cse_recompute_cost(candidate.subtree)
    }

    /// Estimate the cost of maintaining `candidate.bound_summary` as one
    /// continuously-updated shared summary for the life of the workload.
    /// Default: [`default_cse_shared_maintenance_cost`] (a per-family
    /// weight table), applied to whichever field of
    /// `candidate.bound_summary`'s output schema actually carries summary
    /// state (falls back to the cheapest, `Plain`, weight if none does —
    /// e.g. `bound_summary` is a passthrough `KeepPreAsap` node with nothing
    /// summary-shaped to maintain). See `docs/design_docs/cse-cost-model-decision.md`.
    fn cse_shared_maintenance_cost(&self, candidate: &CseCandidate) -> Cost {
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
    /// `docs/design_docs/cse-cost-model-decision.md`): share iff the estimated cost of
    /// maintaining one shared summary is no greater than the estimated total
    /// cost of recomputing it independently everywhere it's used.
    ///
    /// The default body composes [`cse_recompute_cost`](Self::cse_recompute_cost)
    /// and [`cse_shared_maintenance_cost`](Self::cse_shared_maintenance_cost)
    /// — a deployment with real cost knowledge should override those two
    /// (keeping this comparison), or override this method directly for a
    /// wholly different policy.
    fn cse_share_decision(&self, candidate: &CseCandidate) -> ShareDecision {
        let recompute_total = self.cse_recompute_cost(candidate) * candidate.consumer_count;
        let shared = self.cse_shared_maintenance_cost(candidate);
        if shared <= recompute_total {
            ShareDecision::Share
        } else {
            ShareDecision::RecomputeIndependently
        }
    }

    // ── Recurrence-aware costing (issue #287) ───────────────────────────
    //
    // See `crate::recurrence`'s module docs for the full cost model
    // (`maintained_cost_rate`/`recompute_cost_rate` formulas, units,
    // provenance of every new input). The three hooks below are the
    // per-update-event/per-read/per-recomputation cost primitives that
    // formula is built from; `cse_share_decision_with_recurrence` is the
    // composed decision, mirroring how `cse_share_decision` above composes
    // `cse_recompute_cost`/`cse_shared_maintenance_cost`.

    /// Cost of maintaining `candidate`'s bound summary for a single ingest
    /// update event. Units: cost units per update — the
    /// `maintenance_cost_per_update` term of `maintained_cost_rate`
    /// (`crate::recurrence`), where it is multiplied by an `UpdateRate` in
    /// **Hz** (`update_rate * maintenance_cost_per_update`).
    ///
    /// Default: a small nominal constant, `Cost(0.01)` — deliberately
    /// **not** derived from
    /// [`cse_shared_maintenance_cost`](Self::cse_shared_maintenance_cost)'s
    /// per-family weight table. That table's values (~1-6) are calibrated
    /// against [`cse_recompute_cost`](Self::cse_recompute_cost)'s
    /// structural-size proxy for a *life-of-the-workload*, one-time
    /// maintenance magnitude — multiplying them by a real ingest rate (even
    /// a modest one, e.g. 100 events/s) inflates `maintained_cost_rate` far
    /// past any realistic `recompute_cost_rate`, making `Share`
    /// unreachable regardless of how infrequently the summary is actually
    /// read (issue #287 review). `Cost(0.01)` — one order of magnitude
    /// below [`summary_read_cost`](Self::summary_read_cost)'s own nominal
    /// default — reflects only that an incremental per-event update is
    /// normally far cheaper than a full read or recompute, not a measured
    /// ratio; a deployment with a real per-update cost (e.g. observed
    /// sketch-insert latency) should override this instead of relying on
    /// this placeholder.
    fn maintenance_cost_per_update(&self, _candidate: &CseCandidate) -> Cost {
        Cost(0.01)
    }

    /// Cost of one read against `candidate`'s already-maintained summary.
    /// Units: cost units per read — the `summary_read_cost` term of
    /// `maintained_cost_rate`. Default: `Cost(1.0)`, a nominal unit read —
    /// illustrative, like every other numeric default in this trait; a
    /// deployment with a real read-path cost should override this.
    fn summary_read_cost(&self, _candidate: &CseCandidate) -> Cost {
        Cost(1.0)
    }

    /// Cost of recomputing `candidate.subtree` once, from the pre-ASAP/raw
    /// path. Units: cost units per recomputation — the `raw_recompute_cost`
    /// term of `recompute_cost_rate`. Default: delegates to
    /// [`cse_recompute_cost`](Self::cse_recompute_cost) (the same
    /// structural-size proxy `cse_share_decision` already uses).
    fn raw_recompute_cost(&self, candidate: &CseCandidate) -> Cost {
        self.cse_recompute_cost(candidate)
    }

    /// The one-time cost of materializing `candidate`'s bound summary for
    /// the *first* time — before any read or ingest-driven update charges
    /// anything. Units: cost units (a one-time [`Cost`], not a rate).
    ///
    /// This is what makes a purely (or mostly) one-shot comparison
    /// economically sound: without a build cost, "maintained" looked free
    /// to construct, so `Share` won unconditionally for any number of
    /// one-shot consumers, no matter how few (issue #287 review, bug 1).
    /// With it, a single one-shot consumer never benefits from sharing
    /// (build + one read costs more than one direct recompute), while many
    /// one-shot consumers still amortize the fixed build cost across their
    /// reads, same as before.
    ///
    /// Default: delegates to
    /// [`raw_recompute_cost`](Self::raw_recompute_cost) — materializing a
    /// summary for the first time costs about as much as computing its
    /// answer once from raw, since there's no delta history yet to apply
    /// incrementally. A deployment with a distinct measured "cold build"
    /// cost should override this instead.
    fn summary_build_cost(&self, candidate: &CseCandidate) -> Cost {
        self.raw_recompute_cost(candidate)
    }

    /// The recurrence-aware counterpart to
    /// [`cse_share_decision`](Self::cse_share_decision): the same
    /// `Share`/`RecomputeIndependently` choice, weighted by how *often*
    /// `candidate`'s consumers actually run (`recurrence`) instead of only
    /// how many structurally exist (`candidate.consumer_count`). See
    /// `crate::recurrence`'s module docs for the full design.
    ///
    /// - `recurrence.is_empty()` (no [`RepeatingEntry`]/[`DataCharacteristics`]-derived
    ///   metadata available): delegates to
    ///   [`cse_share_decision`](Self::cse_share_decision), preserving
    ///   today's structural-consumer-count behavior exactly — issue #287's
    ///   "preserve existing behavior when recurrence metadata is
    ///   unavailable" requirement.
    /// - Otherwise: compares `maintained_cost_rate` against
    ///   `recompute_cost_rate` (both cost units/second). If
    ///   `recurrence.one_shot_consumers > 0` alongside any recurring rate
    ///   (mixed one-shot + repeating work), `horizon` MUST be `Some` —
    ///   `Err(RecurrenceError::MissingHorizon)` otherwise, per "the cost
    ///   model must not silently combine rate-valued and one-shot costs".
    ///   With no one-shot consumers, `horizon` is optional (comparing bare
    ///   rates is equivalent to comparing `rate * H` for any fixed `H > 0`).
    ///
    /// [`RepeatingEntry`]: asap_types::workload::RepeatingEntry
    /// [`DataCharacteristics`]: asap_types::workload::DataCharacteristics
    fn cse_share_decision_with_recurrence(
        &self,
        candidate: &CseCandidate,
        recurrence: &RecurrenceProfile,
        horizon: Option<Horizon>,
    ) -> Result<RecurrenceCostExplanation, RecurrenceError> {
        recurrence::decide(self, candidate, recurrence, horizon)
    }

    /// Estimate a comparable, numeric cost for one already-constructed
    /// [`ReplacementSubDAG`] candidate at `target` — a real `f64`, not just a
    /// relative rank, meant for a caller that wants to *display* "candidate A
    /// costs ≈ X, candidate B costs ≈ Y" (e.g. a DAG-visualization view built
    /// on [`PlanSpace::cost_sorted`](crate::replacement::PlanSpace::cost_sorted)),
    /// not just order candidates against each other — that ordering job
    /// already belongs to [`rank_candidates`](Self::rank_candidates) (for a
    /// [`SketchAlgorithmStrategy`](crate::replacement::SketchAlgorithmStrategy)
    /// group) and [`cse_share_decision`](Self::cse_share_decision) (for a
    /// [`SharedSubtreeStrategy`](crate::replacement::SharedSubtreeStrategy)
    /// group).
    ///
    /// One method covers both candidate shapes this crate ships:
    /// `candidate.replacement`'s [`Replacement::Summary`] arm (a
    /// `SketchAlgorithmStrategy` candidate — the bound `SummaryNode` is right
    /// there, nothing to reconstruct) and its [`Replacement::Rewrite`] arm
    /// (a `SharedSubtreeStrategy` share-vs-recompute candidate — no bound
    /// `SummaryNode` of its own, since sharing is a decision about a target
    /// already bound some other way; a representative binding is recovered
    /// from `target` itself). `target` is threaded through explicitly
    /// (rather than only ever the target embedded in `candidate` — there
    /// isn't one for a `Rewrite`) so both arms have the `consumer_count`
    /// context a cost estimate needs to be meaningful.
    ///
    /// Default: **not a real cost model** — always returns `f64::NAN`.
    /// `f64::partial_cmp` against `NAN` is always `None`, so a caller that
    /// forgot to check whether its `CostModel` actually overrides this can't
    /// silently treat the placeholder as a real comparison. A deployment
    /// that wants numeric costs exposed should override this method;
    /// [`DefaultCostModel`] does, reusing
    /// [`cse_recompute_cost`](Self::cse_recompute_cost)/
    /// [`cse_shared_maintenance_cost`](Self::cse_shared_maintenance_cost) —
    /// the same arithmetic that already backs `cse_share_decision` — rather
    /// than inventing a second, drifting cost formula.
    fn estimate_cost(&self, candidate: &ReplacementSubDAG, target: &TargetSubDAG<'_>) -> f64 {
        let _ = (candidate, target);
        f64::NAN
    }
}

fn sketch_state(
    node: &SummaryNode,
) -> Option<(&asap_types::post_asap::SketchKind, &GroupingStrategy)> {
    match &node.expr {
        SummaryExpr::SummaryEstimate { summary_input, .. } => sketch_state(summary_input),
        SummaryExpr::SummaryAgg {
            family: SummaryFamilyType::Sketch(kind, grouping),
            ..
        } => Some((kind, grouping)),
        _ => None,
    }
}

fn sketch_state_units(params: &SketchParams) -> f64 {
    match params {
        SketchParams::Cms { width, depth }
        | SketchParams::CountSketch { width, depth }
        | SketchParams::CmsWithHeap { width, depth, .. }
        | SketchParams::CountSketchWithHeap { width, depth, .. } => {
            f64::from(*width) * f64::from(*depth)
        }
        _ => 1.0,
    }
}

fn hydra_grid_cells(params: &HydraParams) -> f64 {
    match params {
        HydraParams::HydraKll { shared_buckets, .. } => f64::from(*shared_buckets),
        HydraParams::HydraCms {
            shared_rows,
            shared_columns,
            ..
        }
        | HydraParams::HydraCountSketch {
            shared_rows,
            shared_columns,
            ..
        } => f64::from(*shared_rows) * f64::from(*shared_columns),
    }
}

/// Apply [`CostModel::rank_candidates`] and enforce its permutation-only
/// contract at the boundary where planner code consumes the result.
pub(crate) fn validated_candidate_ranking(
    cost_model: &dyn CostModel,
    intent: &AggIntent,
    candidates: &[SketchAlgorithm],
) -> Vec<SketchAlgorithm> {
    let ranked = cost_model.rank_candidates(intent, candidates);
    let mut expected = candidates.to_vec();
    let mut actual = ranked.clone();
    expected.sort();
    actual.sort();
    assert_eq!(
        actual, expected,
        "CostModel::rank_candidates must return a permutation of its input; candidate generation is exhaustive and cost models may not add, remove, or duplicate candidates"
    );
    ranked
}

/// The default cost model: preserves [`summary_candidates`]'s built-in static
/// order and [`replacement::default_size_params`]'s built-in sizing unchanged.
///
/// [`summary_candidates`]: crate::replacement::summary_candidates
pub struct DefaultCostModel;

impl CostModel for DefaultCostModel {
    fn rank_candidates(
        &self,
        _intent: &AggIntent,
        candidates: &[SketchAlgorithm],
    ) -> Vec<SketchAlgorithm> {
        candidates.to_vec()
    }

    /// Real numbers, reusing [`CostModel::cse_recompute_cost`]/
    /// [`CostModel::cse_shared_maintenance_cost`] — the same arithmetic
    /// `cse_share_decision`'s default body already composes — rather than a
    /// second formula:
    ///
    /// - [`Replacement::Summary`]: `cse_recompute_cost` (the one-time
    ///   structural cost of building `target` at all) plus
    ///   `cse_shared_maintenance_cost` of the candidate's own bound family
    ///   (a pricier family — a sketch over an exact accumulator, say —
    ///   costs more here, consistent with the per-family weighting
    ///   [`default_cse_shared_maintenance_cost`] already orders candidates
    ///   by).
    /// - [`Replacement::Rewrite`]: recovers one representative bound
    ///   `SummaryNode` for `target` via `realize_child` (the same
    ///   rank-and-take-first helper `replacement::realize_child` reuses for the
    ///   identical need), then charges
    ///   `cse_shared_maintenance_cost` for the candidate that shares
    ///   `target`'s own `Rc` (`Rc::ptr_eq`), or `cse_recompute_cost *
    ///   consumer_count` for the one that doesn't — the same two terms
    ///   `cse_share_decision` already compares against each other. `NaN`
    ///   only if `target` itself can't be bound at all (schema derivation
    ///   failed) — never expected for a target that's already part of a
    ///   legitimate workload tree.
    ///
    ///   **Exception**: a [`ReplacementProvenance::AccuracyReconciliation`]
    ///   candidate (issue #273) never rebuilds `target` — it reads a
    ///   sibling `rc` that this crate builds regardless — so it gets its
    ///   own arm: `cse_shared_maintenance_cost` against `rc`'s **own** bound
    ///   summary (a "read", not a "rebuild `target` per consumer") instead
    ///   of the `cse_recompute_cost * consumer_count` formula the other
    ///   `Rewrite` shapes fall through to. See
    ///   `accuracy_reconciliation.rs`'s own "Costing this candidate shape"
    ///   module docs for why that formula would otherwise misprice it (in
    ///   the wrong direction, worse the more consumers would actually
    ///   benefit from sharing).
    fn estimate_cost(&self, candidate: &ReplacementSubDAG, target: &TargetSubDAG<'_>) -> f64 {
        let consumer_count = target.consumer_count.max(1);
        match &candidate.replacement {
            Replacement::Summary(node) => {
                let cse = CseCandidate {
                    subtree: target.root,
                    bound_summary: node,
                    consumer_count,
                };
                (self.cse_recompute_cost(&cse) + self.cse_shared_maintenance_cost(&cse)).0
            }
            Replacement::Rewrite(rc)
                if candidate.provenance == ReplacementProvenance::AccuracyReconciliation =>
            {
                let Ok(sibling_bound) = realize_child(rc, self) else {
                    return f64::NAN;
                };
                let cse = CseCandidate {
                    subtree: rc,
                    bound_summary: &sibling_bound,
                    // One additional reference into `rc`'s own (already
                    // necessary) build, from this one consumer's
                    // perspective — not `target`'s own `consumer_count`,
                    // which would conflate the reader-side multiplicity
                    // with a maintenance metric that's `rc`'s own group's
                    // concern, not this candidate's.
                    consumer_count: 1,
                };
                self.cse_shared_maintenance_cost(&cse).0
            }
            Replacement::Rewrite(rc) => {
                let Ok(bound) = realize_child(target.root, self) else {
                    return f64::NAN;
                };
                let cse = CseCandidate {
                    subtree: target.root,
                    bound_summary: &bound,
                    consumer_count,
                };
                if Rc::ptr_eq(rc, target.root) {
                    self.cse_shared_maintenance_cost(&cse).0
                } else {
                    (self.cse_recompute_cost(&cse) * consumer_count).0
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replacement::summary_candidates;
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
            candidates: &[SketchAlgorithm],
        ) -> Vec<SketchAlgorithm> {
            let mut v = candidates.to_vec();
            v.reverse();
            v
        }
    }

    #[test]
    fn custom_cost_model_can_reorder_candidates() {
        let intent = default_cardinality();
        let candidates = summary_candidates(&intent);
        let ranked = validated_candidate_ranking(&AlwaysPreferLast, &intent, candidates);
        assert_eq!(ranked.first(), candidates.last());
    }

    struct DropsLast;

    impl CostModel for DropsLast {
        fn rank_candidates(
            &self,
            _intent: &AggIntent,
            candidates: &[SketchAlgorithm],
        ) -> Vec<SketchAlgorithm> {
            candidates[..candidates.len() - 1].to_vec()
        }
    }

    #[test]
    #[should_panic(expected = "must return a permutation of its input")]
    fn candidate_ranking_rejects_filtering() {
        let intent = default_cardinality();
        let candidates = summary_candidates(&intent);
        validated_candidate_ranking(&DropsLast, &intent, candidates);
    }

    struct DuplicatesFirst;

    impl CostModel for DuplicatesFirst {
        fn rank_candidates(
            &self,
            _intent: &AggIntent,
            candidates: &[SketchAlgorithm],
        ) -> Vec<SketchAlgorithm> {
            let mut ranked = candidates.to_vec();
            ranked.push(candidates[0].clone());
            ranked
        }
    }

    #[test]
    #[should_panic(expected = "must return a permutation of its input")]
    fn candidate_ranking_rejects_additions_and_duplicates() {
        let intent = default_cardinality();
        let candidates = summary_candidates(&intent);
        validated_candidate_ranking(&DuplicatesFirst, &intent, candidates);
    }

    /// A deployment that only overrides `rank_candidates` keeps
    /// `asap-plan`'s built-in sizing via the trait's default `size_params`
    /// body — the split is opt-in per method, not all-or-nothing.
    #[test]
    fn size_params_default_body_matches_default_size_params() {
        let intent = default_cardinality();
        assert_eq!(
            AlwaysPreferLast.size_params(SketchAlgorithm::Hll, &intent, 0.01, 0.01),
            crate::replacement::default_size_params(SketchAlgorithm::Hll, &intent, 0.01, 0.01),
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
            candidates: &[SketchAlgorithm],
        ) -> Vec<SketchAlgorithm> {
            candidates.to_vec()
        }

        fn size_params(
            &self,
            kind: SketchAlgorithm,
            intent: &AggIntent,
            eps: f64,
            delta: f64,
        ) -> SketchParams {
            match kind {
                SketchAlgorithm::Kll => {
                    let k = if eps >= 0.01 { 200 } else { 2048 };
                    SketchParams::Kll { k }
                }
                other => crate::replacement::default_size_params(other, intent, eps, delta),
            }
        }
    }

    #[test]
    fn custom_cost_model_can_override_sizing_independently_of_ranking() {
        use asap_types::pre_asap::agg_intent::default_quantile;

        let intent = default_quantile(0.99);
        assert_eq!(
            DiscreteKllRungs.size_params(SketchAlgorithm::Kll, &intent, 0.001, 0.01),
            SketchParams::Kll { k: 2048 },
        );
        // Untouched kinds still fall through to the default formula.
        assert_eq!(
            DiscreteKllRungs.size_params(SketchAlgorithm::Hll, &intent, 0.01, 0.01),
            crate::replacement::default_size_params(SketchAlgorithm::Hll, &intent, 0.01, 0.01),
        );
    }

    // ── CSE sharing (issue #237, #223 stage 4) ──────────────────────────

    use asap_types::post_asap::{
        ExactKind, ExactParams, GroupingStrategy, SketchKind, SummaryExpr, SummaryField,
        SummarySchema,
    };
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
                    expr: SummaryExpr::KeepPreAsap(Rc::new(scan())),
                    schema: SummarySchema {
                        fields: vec![],
                        time_index: None,
                    },
                    guarantee: None,
                }),
                family: family.clone(),
                col: asap_types::pre_asap::expr_ir::ColumnRef::Named("value".into()),
                reduction: asap_types::pre_asap::query_expr::Reduction::by(vec![]),
                grouping: GroupingStrategy::default(),
            },
            schema: SummarySchema {
                fields: vec![SummaryField {
                    name: "state".into(),
                    dtype: family,
                    nullable: false,
                }],
                time_index: None,
            },
            guarantee: None,
        }
    }

    #[test]
    fn default_recompute_cost_is_positive_and_grows_with_structural_size() {
        let leaf = scan();
        let nested = QueryExpr::Dedup {
            cols: vec![0],
            child: std::rc::Rc::new(leaf.clone()),
        };
        assert!(default_cse_recompute_cost(&leaf) > Cost::ZERO);
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
            Cost(3.0),
            "no sharing: Join + 2 independent Scans = 3 unique nodes"
        );
        assert_eq!(
            default_cse_recompute_cost(&with_sharing),
            Cost(2.0),
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
            SketchKind::new(SketchAlgorithm::Hll, SketchParams::Hll { precision: 12 }),
            GroupingStrategy::default(),
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
                candidates: &[SketchAlgorithm],
            ) -> Vec<SketchAlgorithm> {
                candidates.to_vec()
            }
            fn cse_recompute_cost(&self, _candidate: &CseCandidate) -> Cost {
                Cost(1e9)
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

    // ── estimate_cost ────────────────────────────────────────────────────

    /// The trait's default `estimate_cost` body is an explicit placeholder,
    /// not a real cost model — a `CostModel` that only overrides
    /// `rank_candidates` (the minimum required to implement the trait) must
    /// still get `f64::NAN` back, never a value that looks like a real
    /// estimate.
    #[test]
    fn estimate_cost_default_body_is_a_nan_placeholder() {
        struct RankOnly;
        impl CostModel for RankOnly {
            fn rank_candidates(
                &self,
                _intent: &AggIntent,
                candidates: &[SketchAlgorithm],
            ) -> Vec<SketchAlgorithm> {
                candidates.to_vec()
            }
        }

        let root = Rc::new(scan());
        let target = TargetSubDAG::new(&root);
        let candidate = ReplacementSubDAG {
            strategy: "TestStrategy",
            replacement: Replacement::Summary(Rc::new(summary_node(SummaryFamilyType::Plain(
                asap_types::pre_asap::DataType::Float64,
            )))),
            provenance: crate::replacement::ReplacementProvenance::SummaryImplementation,
            rationale: "whatever".into(),
        };
        assert!(RankOnly.estimate_cost(&candidate, &target).is_nan());
    }

    /// `DefaultCostModel::estimate_cost` for a [`Replacement::Summary`]
    /// candidate reuses [`default_cse_shared_maintenance_cost`]'s own
    /// per-family ordering: a candidate bound to a cheap-to-maintain family
    /// (an exact accumulator) must cost less than one bound to an
    /// expensive-to-maintain family (a fitted statistical model), same
    /// target either way — consistent with
    /// `default_shared_maintenance_cost_orders_families_cheapest_to_priciest`
    /// above.
    #[test]
    fn estimate_cost_for_summary_orders_candidates_by_family_cheapest_to_priciest() {
        let root = Rc::new(scan());
        let target = TargetSubDAG::new(&root);

        let cheap = ReplacementSubDAG {
            strategy: "TestStrategy",
            replacement: Replacement::Summary(Rc::new(summary_node(
                SummaryFamilyType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
            ))),
            provenance: crate::replacement::ReplacementProvenance::SummaryImplementation,
            rationale: "exact accumulator".into(),
        };
        let pricey = ReplacementSubDAG {
            strategy: "TestStrategy",
            replacement: Replacement::Summary(Rc::new(summary_node(SummaryFamilyType::StatModel(
                asap_types::post_asap::StatModelKind::Parametric,
                asap_types::post_asap::StatModelParams::Parametric {
                    family: "gaussian_mixture".into(),
                },
            )))),
            provenance: crate::replacement::ReplacementProvenance::SummaryImplementation,
            rationale: "fitted statistical model".into(),
        };

        let cheap_cost = DefaultCostModel.estimate_cost(&cheap, &target);
        let pricey_cost = DefaultCostModel.estimate_cost(&pricey, &target);
        assert!(
            cheap_cost.is_finite() && pricey_cost.is_finite(),
            "cheap={cheap_cost}, pricey={pricey_cost}"
        );
        assert!(
            cheap_cost < pricey_cost,
            "an ExactAggregate candidate should cost less than a StatModel one: \
             exact={cheap_cost}, stat_model={pricey_cost}"
        );
    }

    /// `DefaultCostModel::estimate_cost` for a [`Replacement::Rewrite`] pair
    /// (the `SharedSubtreeStrategy` share-vs-recompute shape) agrees with
    /// what `cse_share_decision` would already pick for the same target: with
    /// many consumers of a cheap-to-recompute leaf, the "share" candidate
    /// (the target's own `Rc`) must cost less than the "recompute
    /// independently" one (a fresh `Rc`) — mirrors
    /// `cse_share_decision_shares_when_recompute_dominates_maintenance`
    /// above, through `estimate_cost` instead of `cse_share_decision`
    /// directly.
    #[test]
    fn estimate_cost_for_rewrite_prefers_sharing_when_recompute_dominates_maintenance() {
        let target_root = Rc::new(scan());
        let target = TargetSubDAG::with_consumer_count(&target_root, 20);

        let share = ReplacementSubDAG {
            strategy: "TestStrategy",
            replacement: Replacement::Rewrite(Rc::clone(&target_root)),
            provenance: crate::replacement::ReplacementProvenance::CseShare,
            rationale: "build once and share".into(),
        };
        let recompute = ReplacementSubDAG {
            strategy: "TestStrategy",
            replacement: Replacement::Rewrite(Rc::new((*target_root).clone())),
            provenance: crate::replacement::ReplacementProvenance::CseRecompute,
            rationale: "build independently".into(),
        };

        let share_cost = DefaultCostModel.estimate_cost(&share, &target);
        let recompute_cost = DefaultCostModel.estimate_cost(&recompute, &target);
        assert!(
            share_cost.is_finite() && recompute_cost.is_finite(),
            "share={share_cost}, recompute={recompute_cost}"
        );
        assert!(
            share_cost < recompute_cost,
            "with 20 consumers of a cheap-to-recompute leaf, sharing should cost less: \
             share={share_cost}, recompute={recompute_cost}"
        );
    }
}
