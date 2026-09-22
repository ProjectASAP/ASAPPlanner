//! `asap-plan` — the cost-aware optimizer layer over the pre-ASAP intent algebra.
//!
//! This crate sits between the language-agnostic IR ([`asap_ir`]) and
//! any runtime: it consumes pre-ASAP [`QueryExpr`](asap_types::pre_asap::QueryExpr)
//! trees and makes the cost-aware decisions the pre-ASAP IR deliberately
//! leaves open — which sketch (if any) realises each approximate intent.
//!
//! **Common sub-expression elimination (CSE) is not this crate's job.**
//! Detection is a primary pass over the pre-ASAP `QueryExpr` IR itself
//! (`asap_types::pre_asap`, design tracked in issue #223), run before a
//! tree ever reaches [`replacement::SketchAlgorithmStrategy`] — see issue #222
//! for why (batch query optimization needs to see shared work across a
//! `QueryWorkload` before summary binding, not after). This crate may
//! eventually run a second, narrower CSE pass of its own over an
//! already-bound `SummaryExpr`/`SummaryNode` DAG, recognizing sharing that's invisible
//! at the pre-ASAP level by construction — e.g. `Quantile(x, 0.99)` and
//! `Quantile(x, 0.95)` are structurally distinct `AggIntent`s but can
//! still share one built sketch, read out twice. That post-ASAP pass is
//! secondary to, and downstream of, the primary pre-ASAP pass, not a
//! replacement for it.
//!
//! It depends only on the IR crate, never on a front end — the layering
//! invariant (arrows point up) holds here too.
//!
//! Post-lowering **canonicalization** is *not* here: it landed in
//! `asap_types::pre_asap::canonicalize`, run inside the shared `resolve_root`
//! so every front end normalizes before the pre-ASAP IR leaves resolution
//! (issue #34, closed).
//!
//! ## Planning workflows
//!
//! Candidate search returns [`PlanSpace`](replacement::PlanSpace), a compact
//! logical choice space with one [`TargetSubDAGCandidates`] per target sub-DAG.
//! [`ReplacementStrategy`] implementations propose local alternatives; search
//! applies the applicable semantic and accuracy checks. Candidate presence does
//! not certify physical deployability or an unknown accuracy guarantee.
//!
//! Integrators choose among these workflows:
//!
//! - Inspect the candidate space, optionally using [`PlanSpace::cost_sorted`]
//!   to obtain ranked views, and perform selection downstream.
//! - Call [`PlanSpace::global_selection`] once for the workload, then
//!   [`GlobalSelection::assemble_selected_dag`] for each query root. This
//!   coordinates logical choices and preserves shared nodes, but makes no
//!   summary-maintenance lifecycle decision.
//! - When Planner owns maintenance-versus-recomputation decisions, use
//!   [`global_selection_with_summary_maintenance_lifecycles`] followed by
//!   [`assemble_selected_dag_with_summary_maintenance_lifecycles`] per root.
//!   This alternative workflow returns [`SummaryMaintenanceLifecyclePlan`]
//!   values containing DAG roots and maintenance decisions; callers do not need
//!   to run ordinary selection/assembly first.
//!
//! Models and evidence determine which choices the helpers can justify.
//! Physical operator binding, placement, storage, deployment, and execution
//! remain downstream responsibilities. Neither taking the first candidate nor
//! assembling a logical DAG creates an executable deployment plan.
//!
//! ## Supporting components
//!
//! - [`cost_model`] — the [`CostModel`](cost_model::CostModel) trait every
//!   deployment's cost-based sketch selection plugs into (issues #6, #33).
//!   `asap-plan` itself only ships [`DefaultCostModel`](cost_model::DefaultCostModel),
//!   which preserves [`replacement`]'s built-in static preference order and
//!   — via [`CostModel::estimate_cost`](cost_model::CostModel::estimate_cost)
//!   — exposes an actual numeric cost per candidate, not just a relative
//!   rank, for a caller (e.g. a DAG-visualization view) that wants to show
//!   "candidate A costs ≈ X" next to "candidate B costs ≈ Y".
//! - [`explanation`] — this crate's explanation of a replacement: a
//!   reporting *view* over [`replacement`]'s candidate-plan space (issue
//!   #257, part of #33) that translates every discovered `TargetSubDAG` with
//!   a non-trivial candidate list into an
//!   [`explanation::ReplacementExplanation`] (why a replacement exists,
//!   where, reusing the candidate's own rationale rather than inventing new
//!   prose), meant for the same downstream consumer (e.g. a
//!   DAG-visualization view) the crate doc's planning workflows section above
//!   already names for [`replacement::PlanSpace`] itself. Superseded PR
//!   #247's own rule-based traversal, which re-walked the tree once per
//!   optimization before [`replacement::search_workload`] existed to read
//!   from instead — see that module's docs for the full reframing.
//! - [`rollup`] — [`rollup::RollupStrategy`] wraps group-by-lattice roll-up
//!   reuse (issue #254, part of #33) as a [`ReplacementStrategy`]: given a
//!   coarser `Aggregate` target and a caller-supplied sibling set, proposes
//!   re-deriving it from an already-computed, strictly finer sibling
//!   `Aggregate` over identical child IR instead of an independent pass
//!   over the raw source — the cross-aggregate sibling of
//!   `pre_asap::cse::share_common_subtrees`'s identical-subtree sharing.
//!   [`rollup::is_legal_rollup_source`] is the standalone legality predicate
//!   other axes (e.g. issue #256's `GroupingStrategy`) are expected to
//!   consult directly, so it and this module's `RollupStrategy` can never
//!   disagree about which siblings qualify.
//! - [`grouping`] — [`grouping::HydraGroupingStrategy`] (issue #256, part of
//!   #33) is an additional `ReplacementStrategy`: the orthogonal
//!   `GroupingStrategy` axis (one summary instance per `by` subpopulation
//!   versus one shared Hydra-family structure serving all of them), offered
//!   alongside the candidates [`replacement::SketchAlgorithmStrategy`]
//!   enumerates for the same target.
//! - [`rewrite`] — the "semantic-equivalent rewriting (e.g. `avg` →
//!   `sum`/`count`) to increase how often the [sharing/sketch] optimizations
//!   above apply" degree of freedom `docs/design_docs/asap_aware_mapping.md`
//!   names (issue #253, part of #33): [`rewrite::AvgToSumOverCountStrategy`]
//!   is a [`replacement::ReplacementStrategy`] that reshapes a bare `avg`
//!   node — which [`replacement::realizations_for_intent`] can only
//!   dispatch to `Realization::PassThrough`, so it can never be a
//!   [`replacement::SharedSubtreeStrategy`] target — into a `sum`/`count`
//!   pair under the same grouping, re-divided back by a wrapping `Project`,
//!   so those *are* ordinary mergeable accumulators sharing/sketching can
//!   reach. It only reshapes; [`replacement::search_workload`]'s cost-based
//!   ranking (or a downstream consumer reading [`replacement::PlanSpace`])
//!   is what decides whether the reshaped form is actually worth picking,
//!   the same propose-don't-decide split every other strategy here keeps.
//!
//! ## Terminology
//!
//! Schema resolution, candidate realization, and runtime placement are distinct stages.
//!
//! | Term | Meaning | Entry point |
//! |---|---|---|
//! | Schema resolution | Derive input schemas and resolve column names to positions | `asap_types::pre_asap::SchemaResolver::resolve_schema`, `resolve_root` |
//! | Realization | Enumerate ranked physical forms for one aggregate intent | `replacement::realizations_for_intent` |
//! | Replacement | Construct each candidate summary sub-DAG | [`replacement::SketchAlgorithmStrategy`] |
//! | Search | Enumerate and compare alternatives across a workload | [`replacement::search_workload`] |
//! | Runtime placement | Choose deployment locations and concrete executors | Downstream physical plan providers |
//!
//! A related question (tracked alongside issues #6/#33): whether this
//! crate should also own a **matching** predicate — "does an already
//! *available* `Realization` satisfy a *required* one" — the way a
//! database's materialized-view matching / "answering queries using
//! views" layer does. It owns the *question*, not an *answer*:
//! [`replacement::Matcher`] is a trait with no default implementation and
//! no shipped instance, the same shape as [`cost_model::CostModel`] and for
//! the same reason — which `Realization`s are actually *available*
//! anywhere is entirely a downstream deployment's concern (an inventory
//! this crate has no way to see), and even the pure sketch-algebra
//! compatibility rules (e.g. a heap-bearing top-k sketch also satisfying a
//! bare frequency point-query) turned out to have deployment-specific
//! competitors (e.g. single-vs-multi-population re-aggregation) that
//! don't reduce to a fact about a summary family's kind alone. `control_plane`'s own
//! `sketch_algebra::capability::Capability`/`is_satisfied_by` is the
//! reference downstream implementation.
//!
//! - [`accuracy`] — the [`AccuracyModel`](accuracy::AccuracyModel) /
//!   [`AccuracyBudgetAllocator`](accuracy::AccuracyBudgetAllocator)
//!   extension points (issue #172): the planning-time algebra that derives
//!   a machine-readable [`ResultGuarantee`](asap_types::post_asap::ResultGuarantee)
//!   for every finalized post-ASAP value, propagates it through
//!   approximate-over-approximate compositions under conservative rules
//!   (no independence assumptions, unknown statistics stay unknown), and
//!   rejects — before any `CostModel` ranks anything — every candidate with
//!   no sound rule or one that misses the applicable `AccuracyTarget`.
//!   Legality and cost are separate responsibilities; see that module's
//!   docs for the pipeline order and the root-vs-per-node precedence rules.

pub mod accuracy;
pub mod accuracy_reconciliation;
pub mod analytical_cost;
pub mod cost_model;
pub mod empirical_comparison;
pub mod empirical_cost;
pub mod empirical_resources;
pub mod erp;
pub mod exact_composition;
pub mod explanation;
mod function_rules;
pub mod grouping;
pub mod pane_sharing;
pub mod physical_handoff_cost;
pub mod physical_operator_statistics;
pub mod physical_plan_cost_model;
pub mod query_physical_lowering;
pub mod recurrence;
pub mod replacement;
pub mod rewrite;
pub mod rollup;
pub mod storage_io;
pub mod summary_maintenance_cost;
pub mod summary_maintenance_dag_export;
pub mod summary_maintenance_lifecycle;
#[cfg(test)]
mod test_support;
pub mod topk_reuse;

pub use accuracy::{
    AccuracyAllocation, AccuracyBudgetAllocator, AccuracyEvidenceProvider, AccuracyModel,
    CompositionShape, DefaultAccuracyModel, EqualSplitAllocator, NoAccuracyEvidence,
    PropagationStats, WorkloadAccuracyEvidence,
};
pub use accuracy_reconciliation::AccuracyReconciliationStrategy;
pub use cost_model::CompleteSummaryCandidateEstimate;
pub use cost_model::{
    maintenance_operation_plan_cost_rate, raw_recompute_cost_rate, read_operation_plan_cost_rate,
    CostModel, CostProvenance, CostUnit, DefaultCostModel, ExactCompositionCostInputs,
    ExactCompositionCostRequest, ValueOperationCapabilities,
};
pub use exact_composition::{ExactComposition, ExactCompositionStrategy, OperationPlacement};
pub use explanation::{
    explain_replacements, explain_replacements_with, ExplanationKind, ReplacementExplanation,
};
pub use grouping::{has_subpopulations, HydraGroupingStrategy};
pub use recurrence::{
    evaluation_rate_of, total_cost, update_rate_from_data_workload, CostRate, EvaluationRate,
    Horizon, RecurrenceCostExplanation, RecurrenceError, RecurrenceProfile, RootRecurrence,
    UpdateRate,
};
pub use replacement::{
    default_strategies, default_strategies_with, search_workload, search_workload_with,
    search_workload_with_targets, summary_candidates, CompositionDecision, GlobalSelection,
    Matcher, PlanSpace, Proposals, RankedTargetSubDAGCandidates, Realization, RealizationError,
    RecurrenceProfileMap, RejectedCandidate, Replacement, ReplacementProvenance,
    ReplacementStrategy, ReplacementSubDAG, SharedSubtreeStrategy, SketchAlgorithmStrategy,
    TargetSubDAG, TargetSubDAGCandidates, TargetSubDAGSelection, MAX_SEARCH_ITERATIONS,
};
pub use rewrite::{AvgToSumOverCountStrategy, SemanticEquivalentRewriteStrategy};
pub use summary_maintenance_dag_export::{
    export_summary_maintenance_plan, SummaryMaintenanceDagExport,
    SummaryMaintenanceDeploymentExport, SummaryMaintenanceLifecycleAlternativeExport,
};
pub use summary_maintenance_lifecycle::{
    assemble_selected_dag_with_summary_maintenance_lifecycles,
    global_selection_with_summary_maintenance_lifecycles, plan_summary_maintenance_lifecycles,
    SummaryMaintenanceCapabilities, SummaryMaintenanceDeployment,
    SummaryMaintenanceLifecycleAlternative, SummaryMaintenanceLifecycleAssemblyError,
    SummaryMaintenanceLifecycleCapabilities, SummaryMaintenanceLifecycleCostInputs,
    SummaryMaintenanceLifecyclePlan, SummaryMaintenanceLifecyclePlanError,
    SummaryMaintenanceLifecycleRejection, SummaryMaintenanceLifecycleSelectionError,
    WorkloadDemand,
};
pub use topk_reuse::TopKLimitReuseStrategy;

pub mod maintained_population;
