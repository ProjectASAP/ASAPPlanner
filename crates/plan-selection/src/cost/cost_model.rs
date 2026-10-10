//! Cost model interface (issues #6, #33).
//!
//! `asap-plan` deliberately has no cost model *implementation* of its own —
//! ranking candidate summaries by real cost (bandwidth budget, memory
//! footprint, site count, observed drift, workload-level CSE credit, …)
//! needs knowledge this crate doesn't have and shouldn't acquire: the crate
//! doc's layering invariant is that `asap-plan` depends only on [`asap_ir`],
//! never on a runtime or a deployment model. What it *can* own is the
//! interface every deployment's cost model plugs into, so selection over the
//! candidates [`replacement`] generates has exactly one extension point.
//! Candidate generation itself does not consult a cost model.
//!
//! [`CostModel::rank_candidates`] is scoped to the approximate-**sketch**
//! family specifically (it takes [`SketchAlgorithm`]s) — `asap_sketch` also has
//! sibling families for sampling-based, wavelet-transform, and fitted
//! statistical-model summaries
//! ([`asap_types::ir::schema::SamplingKind`]/…/[`asap_types::ir::schema::StatModelKind`]),
//! each with its own `(Kind, Params)` pair, deliberately *not* folded into
//! this trait: no core `AggIntent` picks one of those families today, so
//! there is no ranking decision for this trait to own yet. Should a family
//! other than `Sketch` ever need its own ranking, it gets its own trait
//! method rather than overloading this one across incompatible `Kind` types.
//!
//! ## CSE sharing (issue #237, #223 stage 4)
//!
//! [`CseCandidate`]/[`ShareDecision`]/[`CostModel::cse_share_decision`] below
//! decide whether a CSE-detected shared sub-DAG
//! ([`asap_types::ir::cse::share_common_sub_dags`], issue #223 stages
//! 1-2, PR #235) is actually worth sharing, via a real Volcano/Cascades-style
//! cost comparison rather than a fixed rule. See
//! `docs/design_docs/cse-cost-model-decision.md` for the full design discussion (why
//! cost-based, why not a full plan-search engine, the layering constraint
//! that forces detection to stay cost-agnostic).
//! `cost_sorted`
//! (via [`asap_logical_optimizer::pass1::replacement`]'s own `cse_preference`) and
//! [`DefaultCostModel::estimate_cost`] are this crate's own callers.

use std::rc::Rc;

use asap_logical_optimizer::pass1::exact_composition::ExactOperation;
use asap_types::ir::operator::agg_intent::AggIntent;
use asap_types::ir::schema::{
    FieldDataType, GroupingStrategy, HydraParams, SketchAlgorithm, SketchParams,
};
use asap_types::ir::{ASAPOp, Operator, OperatorNode};

use crate::cost::recurrence::{
    self, CostRate, EvaluationRate, Horizon, RecurrenceCostExplanation, RecurrenceError,
    RecurrenceProfile,
};
use asap_logical_optimizer::pass1::exact_composition::{ExactComposition, OperationPlacement};
use asap_logical_optimizer::pass1::replacement::{
    realize_child, Replacement, ReplacementProvenance, ReplacementSubDAG, TargetSubDAG,
};

// ── Recurring-cost vocabulary for mixed exact/summary plans (issue #171) ──

pub use asap_types::cost::CostUnit;

/// Who produced a set of [`ExactCompositionCostInputs`], and under which
/// model version — carried into every composed decision's explanation and
/// DAG export so a reviewer can tell a deployment's measured numbers from
/// a placeholder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostProvenance {
    /// The cost model's own name (e.g. `"DefaultCostModel"`).
    pub model: String,
    /// The model's own version string, whatever scheme it uses.
    pub version: String,
}

/// Which mixed-execution shapes the downstream runtime can actually
/// execute (issue #171). [`asap_logical_optimizer::pass1::exact_composition::ExactCompositionStrategy`]
/// proposes an `ValueOperationAtQueryTime` candidate only when
/// `query_time` is set, and an `ValueOperationAtIngestionTime` candidate only
/// when `ingestion_time` is — a runtime that cannot run an exact
/// operator on the update path must never be handed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ValueOperationCapabilities {
    /// The runtime can apply an exact operator to summary evaluations at
    /// query evaluation time.
    pub query_time: bool,
    /// The runtime can apply an exact row transform on the update path,
    /// feeding its output into maintained summary state.
    pub ingestion_time: bool,
}

impl ValueOperationCapabilities {
    /// Neither shape supported.
    pub const NONE: Self = Self {
        query_time: false,
        ingestion_time: false,
    };
    /// Both shapes supported.
    pub const ALL: Self = Self {
        query_time: true,
        ingestion_time: true,
    };

    pub fn supports(self, placement: OperationPlacement) -> bool {
        match placement {
            OperationPlacement::Read => self.query_time,
            OperationPlacement::Maintenance => self.ingestion_time,
        }
    }
}

/// What [`CostModel::exact_composition_cost_inputs`] is asked about: one
/// composed alternative at one site, paired with the concrete summary it
/// composes with.
#[derive(Debug, Clone, Copy)]
pub struct ExactCompositionCostRequest<'a> {
    /// The target the composed candidate replaces.
    pub target: &'a OperatorNode,
    /// The composition itself — placement, operator, child target.
    pub composition: &'a ExactComposition,
    /// For [`OperationPlacement::Read`]: the child target's *selected*
    /// summary evaluation candidate the exact operator consumes. For
    /// [`OperationPlacement::Maintenance`]: the maintained summary *above* the
    /// transform that consumes its output (the `SummaryAgg` this transform
    /// feeds). Either way, the summary whose maintenance/read cost the
    /// formula charges.
    pub summary: &'a OperatorNode,
    /// How many times this site actually runs once ancestors' own choices
    /// are accounted for (see `candidate_selection::global_selection`).
    pub effective_consumer_count: usize,
}

/// Every input the issue #171 cost formulas need, each individually
/// optional: **an unknown stays `None` — never a zero** — so a formula
/// with a missing input yields no rate at all rather than a spuriously
/// cheap one, and global selection then keeps the conservative
/// keep-as-is behavior. A deployment model that wants defaults supplies
/// them explicitly by overriding [`CostModel::exact_composition_cost_inputs`].
#[derive(Debug, Clone, PartialEq)]
pub struct ExactCompositionCostInputs {
    /// Exact operator cost per row it processes — per evaluation row for a
    /// read-time operation, per input row for an maintenance-time operation.
    pub exact_cost_per_row: Option<f64>,
    /// Rows the exact operator consumes per evaluation (read-time operation) or
    /// per update (transform).
    pub expected_input_rows: Option<f64>,
    /// Rows the exact operator emits per evaluation/update.
    pub expected_output_rows: Option<f64>,
    /// Cost of one update to the composed-with summary's maintained state.
    pub summary_maintenance_cost_per_update: Option<f64>,
    /// Cost of one evaluation of that summary at evaluation time.
    pub summary_read_cost: Option<f64>,
    /// Update (ingest) events per second reaching this site.
    pub update_rate: Option<f64>,
    /// Evaluations per second across every consumer of this site.
    pub evaluation_rate: Option<EvaluationRate>,
    /// Cost of one full raw recompute of the target from pre-ASAP data —
    /// the kept-query baseline's per-evaluation cost.
    pub raw_recompute_cost: Option<f64>,
    /// Recurring formulas require `CostUnitsPerSecond`; totals yield no rate.
    pub unit: CostUnit,
    pub provenance: CostProvenance,
}

impl ExactCompositionCostInputs {
    /// Every input unknown, attributed to `provenance` — what a model that
    /// has no statistics for a site returns.
    pub fn unknown(provenance: CostProvenance) -> Self {
        Self {
            exact_cost_per_row: None,
            expected_input_rows: None,
            expected_output_rows: None,
            summary_maintenance_cost_per_update: None,
            summary_read_cost: None,
            update_rate: None,
            evaluation_rate: None,
            raw_recompute_cost: None,
            unit: CostUnit::CostUnitsPerSecond,
            provenance,
        }
    }

    /// The rate for whichever composition placement is requested —
    /// [`read_operation_plan_cost_rate`] or [`maintenance_operation_plan_cost_rate`].
    pub fn composed_plan_cost_rate(&self, placement: OperationPlacement) -> Option<CostRate> {
        match placement {
            OperationPlacement::Read => read_operation_plan_cost_rate(self),
            OperationPlacement::Maintenance => maintenance_operation_plan_cost_rate(self),
        }
    }
}

/// Outer exact read-time operation over a maintained summary:
///
/// ```text
/// read_operation_plan_cost_rate =
///     update_rate * summary_maintenance_cost_per_update
///   + evaluation_rate * (summary_read_cost
///                        + output_rows_per_eval * exact_read-time operation_cost_per_row)
/// ```
///
/// `None` if any input is unknown — see [`ExactCompositionCostInputs`].
pub fn read_operation_plan_cost_rate(inputs: &ExactCompositionCostInputs) -> Option<CostRate> {
    if inputs.unit != CostUnit::CostUnitsPerSecond {
        return None;
    }
    let maintenance = inputs.update_rate? * inputs.summary_maintenance_cost_per_update?;
    let per_eval =
        inputs.summary_read_cost? + inputs.expected_output_rows? * inputs.exact_cost_per_row?;
    let evaluation = inputs.evaluation_rate?.0 * per_eval;
    finite_rate(maintenance + evaluation)
}

/// Outer maintained summary over an exact maintenance-time operation:
///
/// ```text
/// maintenance_operation_plan_cost_rate =
///     update_rate * (exact_function_cost_per_input_row
///                    + summary_maintenance_cost_per_update)
///   + evaluation_rate * summary_read_cost
/// ```
///
/// `None` if any input is unknown — see [`ExactCompositionCostInputs`].
pub fn maintenance_operation_plan_cost_rate(
    inputs: &ExactCompositionCostInputs,
) -> Option<CostRate> {
    if inputs.unit != CostUnit::CostUnitsPerSecond {
        return None;
    }
    let per_update = inputs.exact_cost_per_row? + inputs.summary_maintenance_cost_per_update?;
    let maintenance = inputs.update_rate? * per_update;
    let evaluation = inputs.evaluation_rate?.0 * inputs.summary_read_cost?;
    finite_rate(maintenance + evaluation)
}

/// The raw/pre-ASAP fallback baseline:
///
/// ```text
/// raw_recompute_cost_rate = evaluation_rate * raw_recompute_cost
/// ```
///
/// `None` if either input is unknown — see [`ExactCompositionCostInputs`].
pub fn raw_recompute_cost_rate(inputs: &ExactCompositionCostInputs) -> Option<CostRate> {
    if inputs.unit != CostUnit::CostUnitsPerSecond {
        return None;
    }
    finite_rate(inputs.evaluation_rate?.0 * inputs.raw_recompute_cost?)
}

fn finite_rate(units_per_second: f64) -> Option<CostRate> {
    units_per_second
        .is_finite()
        .then_some(CostRate(units_per_second))
}

/// A CSE-detected, legality-gated shared sub-DAG with two or more consumers
/// — the unit [`CostModel::cse_share_decision`] decides over. Built by
/// `cost_sorted`
/// (via [`asap_logical_optimizer::pass1::replacement`]'s own `cse_preference`) the first time it
/// needs a representative bound node for a sub-DAG that
/// [`asap_types::ir::cse::share_common_sub_dags`] already collapsed
/// onto one `Rc` for two or more workload roots. See
/// `docs/design_docs/cse-cost-model-decision.md`.
pub struct CseCandidate<'a> {
    /// The shared sub-DAG itself.
    pub sub_dag: &'a Rc<OperatorNode>,
    /// The node this sub-DAG bound to — gives the cost model the
    /// concrete `FieldDataType`/`(kind, params)` actually at stake, not
    /// just the logical shape.
    pub bound_summary: &'a OperatorNode,
    /// How many workload roots reference this exact shared sub-DAG, counted
    /// once up front over the whole workload (always >= 2 — a candidate is
    /// only ever constructed for an actually-shared sub-DAG).
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
    /// Reuse one bound node across every consumer.
    Share,
    /// Bind each occurrence independently — the shared-maintenance cost
    /// isn't worth it for this candidate.
    RecomputeIndependently,
}

/// Default [`CostModel::cse_recompute_cost`]: a structural-size proxy — the
/// number of *unique* nodes in `sub-DAG`'s DAG
/// ([`asap_types::ir::cse::dag_node_count`], the same module this
/// candidate's sharing was detected in). Deliberately **not** a raw
/// `serde_json` serialization length: after CSE, `sub-DAG` is generally a
/// DAG, not a tree (a `CseCandidate` only exists because something got
/// shared), and a naive full serialization re-serializes — over-counts —
/// any descendant `sub-DAG` already shares internally, once per parent
/// that references it, instead of once for the whole DAG. `dag_node_count`
/// dedupes by `Rc` pointer identity, so it charges each unique node's
/// contribution exactly once regardless of how many places within
/// `sub-DAG` reference it. Cheap to compute (one pass, no serialization),
/// and still scales with real structural complexity — a genuinely tiny
/// leaf costs little to recompute, a deep multi-join sub-DAG costs a lot.
/// A deployment with real per-row/per-update cost knowledge should
/// override [`CostModel::cse_recompute_cost`] instead of relying on this.
pub fn default_cse_recompute_cost(sub_dag: &Rc<OperatorNode>) -> Cost {
    Cost(asap_types::ir::cse::dag_node_count(sub_dag) as f64)
}

/// Default [`CostModel::cse_shared_maintenance_cost`]: a small
/// per-[`FieldDataType`] weight, scaled to the same order of magnitude
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
pub fn default_cse_shared_maintenance_cost(family: &FieldDataType) -> Cost {
    const UNIT: f64 = 1.0;
    let weight = match family {
        FieldDataType::Plain(_) => 1.0,
        FieldDataType::ExactAggregate(..) => 1.0,
        FieldDataType::Sketch(..) => 3.0,
        FieldDataType::Sample(..) => 3.0,
        FieldDataType::Wavelet(..) => 5.0,
        FieldDataType::StatModel(..) => 6.0,
    };
    Cost(weight * UNIT)
}

/// Selection-time preferences and costs over the candidates Stage 1
/// generates.
///
/// [`replacement::summary_candidates`] returns every algorithm that *can* answer an
/// intent, in an arbitrary static preference order (issue #98's "one home"
/// for the candidate set), and candidate generation keeps that order. A
/// `CostModel` re-orders the candidates when they are selected, under real,
/// deployment-specific cost knowledge this crate has no way to know about.
pub trait CostModel {
    /// Whether [`Self::candidate_cost`] prices a complete physical
    /// alternative, including its raw baseline, rather than a local
    /// heuristic for one memo-group node.
    ///
    /// Complete-plan models make every CSE alternative available to final
    /// ranking. Choosing a share/recompute arm first through the legacy
    /// structural hooks would discard a physical alternative before its
    /// evidence-backed cost was compared.
    fn candidate_cost_covers_complete_plan(&self) -> bool {
        false
    }

    /// Opt into historical qualitative ranking/CSE decisions when no numeric
    /// candidate cost exists. The safe default leaves an uncosted candidate
    /// unselected; a model with an intentional non-numeric policy overrides
    /// this to `true`.
    fn allow_uncosted_legacy_selection(&self) -> bool {
        false
    }

    /// Candidate-level cost availability for final selection. A non-finite or
    /// negative estimate is unknown/invalid, never an available cost.
    fn candidate_cost(
        &self,
        candidate: &ReplacementSubDAG,
        target: &TargetSubDAG<'_>,
    ) -> Option<Cost> {
        let value = self.estimate_cost(candidate, target);
        (value.is_finite() && value >= 0.0).then_some(Cost(value))
    }

    /// Rank `candidates` (as returned by
    /// [`summary_candidates`](asap_logical_optimizer::pass1::replacement::summary_candidates)) for
    /// `intent`, best choice first.
    ///
    /// Implementations MAY reorder freely, but MUST return exactly the input
    /// candidates: no additions, removals, or duplicates. Semantic legality
    /// belongs to replacement generation; cost availability is reported
    /// separately by `candidate_cost`. Letting this hook filter would violate
    /// [`ReplacementStrategy`]'s exhaustive, never-prune contract. This
    /// invariant is checked at every production call site.
    ///
    /// [`ReplacementStrategy`]: asap_logical_optimizer::pass1::replacement::ReplacementStrategy
    fn rank_candidates(
        &self,
        intent: &AggIntent,
        candidates: &[SketchAlgorithm],
    ) -> Vec<SketchAlgorithm>;

    /// Estimated number of distinct subpopulations produced by `target`'s
    /// grouping keys. `None` means the deployment has no cardinality estimate;
    /// grouping alternatives remain legal but keep their discovery order.
    fn estimated_subpopulation_count(&self, _target: &OperatorNode) -> Option<usize> {
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
        let Replacement::SubDAG(node) = &candidate.replacement else {
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

    /// Estimate the one-time cost of recomputing `candidate.sub-DAG`
    /// independently at a single use site. Default:
    /// [`default_cse_recompute_cost`] (a structural-size proxy). See
    /// `docs/design_docs/cse-cost-model-decision.md`.
    fn cse_recompute_cost(&self, candidate: &CseCandidate) -> Cost {
        default_cse_recompute_cost(candidate.sub_dag)
    }

    /// Estimate the cost of maintaining `candidate.bound_summary` as one
    /// continuously-updated shared summary for the life of the workload.
    /// Default: [`default_cse_shared_maintenance_cost`] (a per-family
    /// weight table), applied to whichever field of
    /// `candidate.bound_summary`'s output schema actually carries summary
    /// state (falls back to the cheapest, `Plain`, weight if none does —
    /// e.g. `bound_summary` is a kept non-ASAP sub-DAG with nothing
    /// summary-shaped to maintain). See `docs/design_docs/cse-cost-model-decision.md`.
    fn cse_shared_maintenance_cost(&self, candidate: &CseCandidate) -> Cost {
        let family = candidate
            .bound_summary
            .schema
            .fields
            .iter()
            .map(|f| &f.dtype)
            .find(|dtype| !matches!(dtype, FieldDataType::Plain(_)))
            .cloned()
            .unwrap_or(FieldDataType::Plain(
                asap_types::ir::schema::DataType::Float64,
            ));
        default_cse_shared_maintenance_cost(&family)
    }

    /// Decide whether to reuse one shared node across every
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
    // See `crate::cost::recurrence`'s module docs for the full cost model
    // (`maintained_cost_rate`/`recompute_cost_rate` formulas, units,
    // provenance of every new input). The three hooks below are the
    // per-update-event/per-read/per-recomputation cost primitives that
    // formula is built from; `cse_share_decision_with_recurrence` is the
    // composed decision, mirroring how `cse_share_decision` above composes
    // `cse_recompute_cost`/`cse_shared_maintenance_cost`.

    /// Cost of maintaining `candidate`'s bound summary for a single ingest
    /// update event. Units: cost units per update — the
    /// `maintenance_cost_per_update` term of `maintained_cost_rate`
    /// (`crate::cost::recurrence`), where it is multiplied by an `UpdateRate` in
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

    /// Cost of recomputing `candidate.sub-DAG` once, from the pre-ASAP/raw
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
    /// `crate::cost::recurrence`'s module docs for the full design.
    ///
    /// - `recurrence.is_empty()` (no [`RepeatingEntry`]/[`DataWorkload`]-derived
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
    /// [`DataWorkload`]: asap_types::workload::DataWorkload
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
    /// on `cost_sorted`),
    /// not just order candidates against each other — that ordering job
    /// already belongs to [`rank_candidates`](Self::rank_candidates) (for a
    /// [`ASAPStrategies`](asap_logical_optimizer::pass1::replacement::ASAPStrategies)
    /// group) and [`cse_share_decision`](Self::cse_share_decision) (for a
    /// [`SharedSubDAGStrategy`](asap_logical_optimizer::pass1::replacement::SharedSubDAGStrategy)
    /// group).
    ///
    /// One method covers both candidate shapes this crate ships:
    /// `candidate.replacement`'s [`Replacement::SubDAG`] from a summary
    /// realization (a `ASAPStrategies` candidate — the bound node is
    /// right there, nothing to reconstruct) and the same arm from a rewrite
    /// (a `SharedSubDAGStrategy` share-vs-recompute candidate — no bound
    /// summary of its own, since sharing is a decision about a target
    /// already bound some other way; a representative binding is recovered
    /// from `target` itself). `target` is threaded through explicitly
    /// (rather than only ever the target embedded in `candidate` — there
    /// isn't one for a rewrite) so both arms have the `consumer_count`
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

    /// Physical feasibility evidence for a complete summary candidate.
    /// `None` defers admission to physical/deployment compilation; `Some(false)`
    /// excludes the candidate without changing its computation or parameters.
    fn summary_support_evidence(&self, _summary: &OperatorNode) -> Option<bool> {
        None
    }

    /// Which mixed exact/summary execution shapes the downstream runtime
    /// advertises (issue #171). Gates candidate *generation* in
    /// [`asap_logical_optimizer::pass1::exact_composition::ExactCompositionStrategy`]: a shape the
    /// runtime can't execute is never proposed, so it can't be selected
    /// either.
    ///
    /// Default: [`ValueOperationCapabilities::ALL`]. This describes shapes
    /// worth exploring, not proof that a runtime implements them. The
    /// [`Self::value_operation_support_evidence`] hook gates selection;
    /// a deployment whose runtime lacks a shape can narrow this hook.
    fn value_operation_capabilities(&self) -> ValueOperationCapabilities {
        ValueOperationCapabilities::ALL
    }

    /// Whether the runtime implements this concrete function at this
    /// placement. Deployments override this definition-level hook when
    /// support differs between functions; the default delegates to the
    /// coarse placement capability for backward compatibility.
    fn supports_value_operation(
        &self,
        _operation: &ExactOperation,
        placement: OperationPlacement,
    ) -> bool {
        self.value_operation_capabilities().supports(placement)
    }

    /// Runtime support evidence for a mixed exact/summary operation. `None`
    /// means that the logical shape is possible but runtime support has not
    /// been established. The legacy boolean hook still rules out explicit
    /// `false`; implementations that can prove support override this method
    /// with `Some(true)`.
    fn value_operation_support_evidence(
        &self,
        operation: &ExactOperation,
        placement: OperationPlacement,
    ) -> Option<bool> {
        (!self.supports_value_operation(operation, placement)).then_some(false)
    }

    /// The statistics the issue #171 recurring-cost formulas need for one
    /// composed alternative — see [`ExactCompositionCostInputs`] for each
    /// input and [`read_operation_plan_cost_rate`]/
    /// [`maintenance_operation_plan_cost_rate`]/[`raw_recompute_cost_rate`] for how
    /// they combine. One structured hook rather than eight scalar ones, so
    /// a deployment answers them all from one place (and can attach its own
    /// [`CostProvenance`]).
    ///
    /// Default: every input unknown ([`ExactCompositionCostInputs::unknown`])
    /// — unknown is never zero, and with no rate derivable
    /// `candidate_selection::global_selection` keeps the conservative keep-as-is
    /// behavior for the site. A deployment that wants defaults must supply
    /// them here explicitly.
    fn exact_composition_cost_inputs(
        &self,
        request: &ExactCompositionCostRequest<'_>,
    ) -> ExactCompositionCostInputs {
        let _ = request;
        ExactCompositionCostInputs::unknown(CostProvenance {
            model: "CostModel::exact_composition_cost_inputs (default)".into(),
            version: "unknown".into(),
        })
    }
}

fn sketch_state(
    node: &OperatorNode,
) -> Option<(&asap_types::ir::schema::SketchKind, &GroupingStrategy)> {
    match &node.operator {
        Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) => {
            sketch_state(summary_input)
        }
        Operator::ASAP(ASAPOp::SummaryAgg {
            family: FieldDataType::Sketch(kind, grouping),
            ..
        }) => Some((kind, grouping)),
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

/// The default cost model: preserves [`summary_candidates`]'s built-in static
/// order.
///
/// [`summary_candidates`]: asap_logical_optimizer::pass1::replacement::summary_candidates
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
    /// - A [`ReplacementProvenance::SummaryRealization`] candidate (a
    ///   `ASAPStrategies` binding): `cse_recompute_cost` (the one-time
    ///   structural cost of building `target` at all) plus
    ///   `cse_shared_maintenance_cost` of the candidate's own bound family
    ///   (a pricier family — a sketch over an exact accumulator, say —
    ///   costs more here, consistent with the per-family weighting
    ///   [`default_cse_shared_maintenance_cost`] already orders candidates
    ///   by).
    /// - Any other [`Replacement::SubDAG`] (a logical rewrite or a CSE
    ///   share/recompute candidate): recovers one
    ///   representative bound node for `target` via `realize_child` (the same
    ///   rank-and-take-first helper `replacement::realize_child` reuses for the
    ///   identical need), then charges
    ///   `cse_shared_maintenance_cost` for the candidate that shares
    ///   `target`'s own `Rc` (`Rc::ptr_eq`), or `cse_recompute_cost *
    ///   consumer_count` for the one that doesn't — the same two terms
    ///   `cse_share_decision` already compares against each other. `NaN`
    ///   only if `target` itself can't be bound at all (schema derivation
    ///   failed) — never expected for a target that's already part of a
    ///   legitimate workload DAG.
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
            Replacement::SubDAG(node)
                if candidate.provenance == ReplacementProvenance::SummaryRealization =>
            {
                let cse = CseCandidate {
                    sub_dag: target.root,
                    bound_summary: node,
                    consumer_count,
                };
                (self.cse_recompute_cost(&cse) + self.cse_shared_maintenance_cost(&cse)).0
            }
            Replacement::SubDAG(rc)
                if candidate.provenance == ReplacementProvenance::AccuracyReconciliation =>
            {
                let Ok(sibling_bound) = realize_child(rc) else {
                    return f64::NAN;
                };
                let cse = CseCandidate {
                    sub_dag: rc,
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
            Replacement::SubDAG(rc) => {
                let Ok(bound) = realize_child(target.root) else {
                    return f64::NAN;
                };
                let cse = CseCandidate {
                    sub_dag: target.root,
                    bound_summary: &bound,
                    consumer_count,
                };
                if Rc::ptr_eq(rc, target.root) {
                    self.cse_shared_maintenance_cost(&cse).0
                } else {
                    (self.cse_recompute_cost(&cse) * consumer_count).0
                }
            }
            // A composed candidate is costed in cost-units-per-second by
            // `candidate_selection::global_selection` against the child decision it
            // is committed with — a different unit from this structural
            // estimate, and unknowable here without that child. `NaN`
            // keeps it from ever out-ranking a real estimate by accident.
            Replacement::ExactComposition(_) => f64::NAN,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_logical_optimizer::pass1::replacement::summary_candidates;
    use asap_types::ir::operator::agg_intent::default_cardinality;

    #[test]
    fn default_cost_model_preserves_static_order() {
        let intent = default_cardinality();
        let candidates = summary_candidates(&intent);
        assert_eq!(
            DefaultCostModel.rank_candidates(&intent, candidates),
            candidates.to_vec()
        );
    }

    // ── Recurring-cost formulas (issue #171) ─────────────────────────────

    fn known_inputs() -> ExactCompositionCostInputs {
        ExactCompositionCostInputs {
            exact_cost_per_row: Some(0.1),
            expected_input_rows: Some(50.0),
            expected_output_rows: Some(10.0),
            summary_maintenance_cost_per_update: Some(0.01),
            summary_read_cost: Some(1.0),
            update_rate: Some(100.0),
            evaluation_rate: Some(EvaluationRate(2.0)),
            raw_recompute_cost: Some(100.0),
            unit: CostUnit::CostUnitsPerSecond,
            provenance: CostProvenance {
                model: "test".into(),
                version: "1".into(),
            },
        }
    }

    #[test]
    fn composition_formulas_match_the_issue_definitions() {
        let inputs = known_inputs();
        // 100 * 0.01 + 2 * (1 + 10 * 0.1) = 1 + 4 = 5
        assert_eq!(read_operation_plan_cost_rate(&inputs).unwrap().0, 5.0);
        // 100 * (0.1 + 0.01) + 2 * 1 = 11 + 2 = 13
        assert!((maintenance_operation_plan_cost_rate(&inputs).unwrap().0 - 13.0).abs() < 1e-9);
        // 2 * 100
        assert_eq!(raw_recompute_cost_rate(&inputs).unwrap().0, 200.0);
        assert_eq!(
            crate::cost::recurrence::total_cost(CostRate(5.0), Horizon(10.0), Cost(3.0)),
            Cost(53.0)
        );
    }

    /// Consolidation preserves type identity and rejects totals in recurring formulas.
    #[test]
    fn recurring_formulas_require_the_shared_rate_unit() {
        let mut inputs = known_inputs();
        let shared: asap_types::cost::CostUnit = inputs.unit;
        assert_eq!(shared.as_str(), "cost_units_per_second");
        inputs.unit = asap_types::cost::CostUnit::CostUnits;
        assert_eq!(read_operation_plan_cost_rate(&inputs), None);
        assert_eq!(maintenance_operation_plan_cost_rate(&inputs), None);
        assert_eq!(raw_recompute_cost_rate(&inputs), None);
    }

    #[test]
    fn a_missing_input_yields_no_rate_not_zero() {
        let mut inputs = known_inputs();
        inputs.summary_maintenance_cost_per_update = None;
        assert_eq!(read_operation_plan_cost_rate(&inputs), None);
        assert_eq!(maintenance_operation_plan_cost_rate(&inputs), None);
        // The baseline doesn't need maintenance and is still known.
        assert!(raw_recompute_cost_rate(&inputs).is_some());
        let unknown = ExactCompositionCostInputs::unknown(known_inputs().provenance);
        assert_eq!(raw_recompute_cost_rate(&unknown), None);
    }

    #[test]
    fn default_model_has_potential_shapes_but_unknown_runtime_support() {
        assert_eq!(
            DefaultCostModel.value_operation_capabilities(),
            ValueOperationCapabilities::ALL
        );
        assert_eq!(
            DefaultCostModel.value_operation_support_evidence(
                &ExactOperation::Aggregate {
                    reduction: asap_types::ir::operator::operator_properties::Reduction::by(vec![]),
                    measures: vec![AggIntent::Max { col: None }],
                    output_names: vec![],
                    filters: vec![],
                    having: None,
                },
                OperationPlacement::Read,
            ),
            None
        );
        assert!(ValueOperationCapabilities::NONE
            .supports(OperationPlacement::Read)
            .not());
    }

    trait Not {
        fn not(self) -> bool;
    }
    impl Not for bool {
        fn not(self) -> bool {
            !self
        }
    }

    // ── CSE sharing (issue #237, #223 stage 4) ──────────────────────────

    use asap_types::ir::operator::operator_properties::Source;
    use asap_types::ir::schema::DataType;
    use asap_types::ir::schema::{
        ExactKind, ExactParams, Field, GroupingStrategy, Schema, SketchKind,
    };
    use asap_types::ir::{NonASAPOp, Predicate, ScalarExpr};

    fn scan() -> Rc<OperatorNode> {
        OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Field::plain("ts", DataType::Timestamp, false),
                    Field::plain("value", DataType::Float64, false),
                ],
                0,
                vec![],
            ),
        }))
        .unwrap()
    }

    /// A `SummaryAgg` directly over the kept `scan()` sub-DAG.
    fn summary_node(family: FieldDataType) -> Rc<OperatorNode> {
        std::rc::Rc::new(
            OperatorNode::with_schema(
                asap_types::ir::Operator::ASAP(ASAPOp::SummaryAgg {
                    child: scan(),
                    family: family.clone(),
                    input: asap_types::ir::schema::SummaryUpdate::column(
                        asap_types::ir::scalar::ColumnRef::Named("value".into()),
                    ),
                    reduction: asap_types::ir::operator::operator_properties::Reduction::by(vec![]),
                    grouping: GroupingStrategy::default(),
                    filter: None,
                }),
                Schema::lifted(vec![Field::new("state", family, false)], None),
            )
            .with_guarantee(None),
        )
    }

    #[test]
    fn default_recompute_cost_is_positive_and_grows_with_structural_size() {
        let leaf = scan();
        let nested =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Dedup {
                cols: vec![0],
                child: Rc::clone(&leaf),
            }))
            .unwrap();
        assert!(default_cse_recompute_cost(&leaf) > Cost::ZERO);
        assert!(default_cse_recompute_cost(&nested) > default_cse_recompute_cost(&leaf));
    }

    /// The DAG-awareness this proxy exists for: a sub-DAG that internally
    /// re-references one shared descendant (e.g. after single-query CSE,
    /// `x op x` collapsing both branches onto one `Rc`) must cost the same
    /// as if that descendant only appeared once — not double, the way a
    /// naive per-path size measure (a full serialization, or an
    /// identity-blind recursive walk) would count it.
    #[test]
    fn default_recompute_cost_does_not_double_count_an_internally_shared_descendant() {
        use asap_types::ir::operator::operator_properties::JoinKind;
        use asap_types::ir::scalar::ScalarValue;

        let true_pred = || Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true)));
        let shared_leaf = scan();
        let no_sharing =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Join {
                kind: JoinKind::Inner,
                pred: true_pred(),
                left: scan(),
                right: scan(),
            }))
            .unwrap();
        let with_sharing =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Join {
                kind: JoinKind::Inner,
                pred: true_pred(),
                left: Rc::clone(&shared_leaf),
                right: Rc::clone(&shared_leaf),
            }))
            .unwrap();
        assert_eq!(
            default_cse_recompute_cost(&no_sharing),
            Cost(3.0),
            "no sharing: Join + 2 independent Scans = 3 unique nodes"
        );
        assert_eq!(
            default_cse_recompute_cost(&with_sharing),
            Cost(2.0),
            "internal sharing: Join + 1 shared Scan (referenced twice) = \
             2 unique nodes, not 3 — a per-path size measure would \
             wrongly charge for the shared Scan twice"
        );
    }

    #[test]
    fn default_shared_maintenance_cost_orders_families_cheapest_to_priciest() {
        let exact = default_cse_shared_maintenance_cost(&FieldDataType::ExactAggregate(
            ExactKind::Sum,
            ExactParams::Sum,
        ));
        let sketch = default_cse_shared_maintenance_cost(&FieldDataType::Sketch(
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
            sub_dag: &scan(),
            bound_summary: &summary_node(FieldDataType::ExactAggregate(
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
            sub_dag: &scan(),
            bound_summary: &summary_node(FieldDataType::StatModel(
                asap_types::ir::schema::StatModelKind::Parametric,
                asap_types::ir::schema::StatModelParams::Parametric {
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
            sub_dag: &scan(),
            bound_summary: &summary_node(FieldDataType::StatModel(
                asap_types::ir::schema::StatModelKind::Parametric,
                asap_types::ir::schema::StatModelParams::Parametric {
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

        let root = scan();
        let target = TargetSubDAG::new(&root);
        let candidate = ReplacementSubDAG {
            strategy: "TestStrategy",
            replacement: Replacement::SubDAG(summary_node(FieldDataType::Plain(
                asap_types::ir::schema::DataType::Float64,
            ))),
            provenance: asap_logical_optimizer::pass1::replacement::ReplacementProvenance::SummaryRealization,
            rationale: "whatever".into(),
        };
        assert!(RankOnly.estimate_cost(&candidate, &target).is_nan());
    }

    /// `DefaultCostModel::estimate_cost` for a summary-rooted [`Replacement::SubDAG`]
    /// candidate reuses [`default_cse_shared_maintenance_cost`]'s own
    /// per-family ordering: a candidate bound to a cheap-to-maintain family
    /// (an exact accumulator) must cost less than one bound to an
    /// expensive-to-maintain family (a fitted statistical model), same
    /// target either way — consistent with
    /// `default_shared_maintenance_cost_orders_families_cheapest_to_priciest`
    /// above.
    #[test]
    fn estimate_cost_for_summary_orders_candidates_by_family_cheapest_to_priciest() {
        let root = scan();
        let target = TargetSubDAG::new(&root);

        let cheap = ReplacementSubDAG {
            strategy: "TestStrategy",
            replacement: Replacement::SubDAG(summary_node(FieldDataType::ExactAggregate(
                ExactKind::Sum,
                ExactParams::Sum,
            ))),
            provenance: asap_logical_optimizer::pass1::replacement::ReplacementProvenance::SummaryRealization,
            rationale: "exact accumulator".into(),
        };
        let pricey = ReplacementSubDAG {
            strategy: "TestStrategy",
            replacement: Replacement::SubDAG(summary_node(FieldDataType::StatModel(
                asap_types::ir::schema::StatModelKind::Parametric,
                asap_types::ir::schema::StatModelParams::Parametric {
                    family: "gaussian_mixture".into(),
                },
            ))),
            provenance: asap_logical_optimizer::pass1::replacement::ReplacementProvenance::SummaryRealization,
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

    /// `DefaultCostModel::estimate_cost` for a relational [`Replacement::SubDAG`] pair
    /// (the `SharedSubDAGStrategy` share-vs-recompute shape) agrees with
    /// what `cse_share_decision` would already pick for the same target: with
    /// many consumers of a cheap-to-recompute leaf, the "share" candidate
    /// (the target's own `Rc`) must cost less than the "recompute
    /// independently" one (a fresh `Rc`) — mirrors
    /// `cse_share_decision_shares_when_recompute_dominates_maintenance`
    /// above, through `estimate_cost` instead of `cse_share_decision`
    /// directly.
    #[test]
    fn estimate_cost_for_rewrite_prefers_sharing_when_recompute_dominates_maintenance() {
        let target_root = scan();
        let target = TargetSubDAG::with_consumer_count(&target_root, 20);

        let share = ReplacementSubDAG {
            strategy: "TestStrategy",
            replacement: Replacement::SubDAG(Rc::clone(&target_root)),
            provenance: asap_logical_optimizer::pass1::replacement::ReplacementProvenance::CseShare,
            rationale: "build once and share".into(),
        };
        let recompute = ReplacementSubDAG {
            strategy: "TestStrategy",
            replacement: Replacement::SubDAG(Rc::new((*target_root).clone())),
            provenance:
                asap_logical_optimizer::pass1::replacement::ReplacementProvenance::CseRecompute,
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
