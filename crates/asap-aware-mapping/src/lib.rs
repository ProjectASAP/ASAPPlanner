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
//! ## Status
//!
//! This crate has two replacement/search capabilities — and deliberately no
//! third one that commits to a single, final, physically-materialized
//! answer for a whole workload:
//!
//! - [`replacement::ReplacementStrategy`] — per-target, never prunes,
//!   exhaustive. The `TargetSubDAG`/`ReplacementSubDAG`/
//!   `ReplacementStrategy` vocabulary `docs/design_docs/asap_aware_mapping.md` stubs out
//!   under "Key concepts (not yet implemented)", implemented for real (issue
//!   #251, part of #33). [`replacement::SketchAlgorithmStrategy::replacements`]
//!   both *decides* what an `AggIntent` may become
//!   ([`replacement::implementations_for_with`], exhaustive and ranked via a
//!   `CostModel`, sized to the `AccuracyTarget`) and *constructs* each
//!   candidate's bound [`SummaryNode`](asap_types::post_asap::SummaryNode) —
//!   every candidate comes back, not just one.
//!   [`replacement::SharedSubtreeStrategy`] does the analogous job for the
//!   build-independently-vs-build-once-and-share choice at a CSE-detected
//!   shared subtree.
//! - [`replacement::search_workload`]/[`replacement::PlanSpace::cost_sorted`]
//!   — workload-wide, every candidate + cost, never materializes one
//!   physical answer. Merged into the same module (issue #252, part of
//!   #33): [`replacement::search_workload`]/[`replacement::search_workload_with`]
//!   *search* — discover every candidate `TargetSubDAG` across a whole
//!   workload (not just one target in isolation) and run every registered
//!   strategy against each one, to a fixpoint, without ever materializing a
//!   flat `2^N`-sized candidate-plan list: [`replacement::PlanSpace`] holds
//!   one Cascades-style [`replacement::MemoGroup`] per distinct
//!   `TargetSubDAG`, each carrying every alternative discovered for it.
//!   [`replacement::PlanSpace::cost_sorted`] is the final
//!   `sorted_by(cost_model)` step, ranking each group's candidates
//!   best-first via the same [`CostModel`](cost_model::CostModel) the
//!   single-target steps above already consult — see [`replacement`]'s own
//!   module docs for the full design (MEMO groups vs. flat plans, dedup
//!   discipline, termination, cost-based ranking).
//!
//! **Picking *which* candidate, and materializing one final answer, is a
//! downstream deployment's job, out of this crate's scope.** This crate's
//! output boundary is [`replacement::PlanSpace`]: every candidate
//! replacement plus its cost, meant for a downstream consumer (e.g. a
//! DAG-visualization view, or a deployment's own physical binder). Which
//! sketch to commit to *and* where to place it are a joint decision only a
//! deployment can see the full picture for — picking one in isolation, with
//! no real consumer of that single materialized answer inside this crate,
//! is out of scope. (A prior workload-wide "keep first/cost-preferred
//! candidate per node, memoized by `Rc` identity" entry point —
//! `bind::implement_workload`/`implement_workload_with` — used to live here
//! and was removed for exactly this reason; see the terminology table below
//! for where that "one answer" step now belongs, downstream.)
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
//!   DAG-visualization view) the crate doc's `## Status` section above
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
//!   node — which [`replacement::implementations_for_with`] can only
//!   dispatch to `Implementation::PassThrough`, so it can never be a
//!   [`replacement::SharedSubtreeStrategy`] target — into a `sum`/`count`
//!   pair under the same grouping, re-divided back by a wrapping `Project`,
//!   so those *are* ordinary mergeable accumulators sharing/sketching can
//!   reach. It only reshapes; [`replacement::search_workload`]'s cost-based
//!   ranking (or a downstream consumer reading [`replacement::PlanSpace`])
//!   is what decides whether the reshaped form is actually worth picking,
//!   the same propose-don't-decide split every other strategy here keeps.
//!
//! ## Terminology — "bind" already means three different things nearby;
//! this crate's own logical→physical step is named "implementation" instead
//!
//! `asap-plan` and its downstream consumers (e.g. `ASAPQuery-backend`'s
//! `control_plane`) independently reused the word "bind" for three
//! *different*, layer-specific meanings — none of which is what this
//! crate's own [`replacement`] module does. To avoid becoming a
//! fourth, colliding sense of the same word, this crate names its own
//! logical intent → physical realization step after the term the
//! query-optimization literature already uses for exactly that step:
//! **implementation** (Cascades/Volcano's "implementation rule", logical →
//! physical, as distinct from a *transformation rule*, logical → logical —
//! see Graefe, *The Cascades Framework for Query Optimization*).
//!
//! | Term | Stage | Meaning | Lives in |
//! |---|---|---|---|
//! | **Parse** | parse | text (PromQL/SQL) → AST | `asap-frontend-promql` / `asap-frontend-sql` |
//! | **Bind #1** | name resolution | `ColumnRef` (a name) → `ColumnId` (a concrete schema column) — the classic RDBMS "Parse → **Bind** → Optimize" pipeline sense (e.g. SQL Server's query-processor terminology) | [`asap_types::pre_asap::binder::Binder`](https://docs.rs/asap-types) |
//! | **Implementation** — `replacement::implementations_for_with` | pre-ASAP → post-ASAP, *one node* | enumerating every concrete physical realization (a sketch family, an exact accumulator, or pass-through) for one [`AggIntent`](asap_types::pre_asap::agg_intent::AggIntent) | [`replacement`] |
//! | **Replacement** — [`replacement::SketchAlgorithmStrategy::replacements`] | pre-ASAP → post-ASAP, *one target, every candidate* | wrap each `implementations_for_with` candidate into its own bound [`SummaryNode`](asap_types::post_asap::SummaryNode), ranked — a caller wanting one answer takes the first entry itself | [`replacement`] |
//! | **Search** — [`replacement::search_workload`]/[`replacement::search_workload_with`] | pre-ASAP → post-ASAP, *whole workload, every candidate* | a Cascades/Volcano-style MEMO search: discover every candidate `TargetSubDAG` across a whole workload (not just one target in isolation), run every registered `ReplacementStrategy` against each to a fixpoint, and dedup into a [`replacement::PlanSpace`] — one [`replacement::MemoGroup`] per distinct `TargetSubDAG` holding every alternative discovered for it, never a flat `2^N`-sized list of whole candidate plans | [`replacement`] |
//! | **Bind #2** (downstream, not in this crate) | post-ASAP → deployment placement | a *deployment's* own physical binder, deciding **which** candidate to commit to *and* **placement** (edge vs. backend, wire format, …) for a whole workload — a genuinely different, deployment-specific decision this crate doesn't model at all (this is also where a prior workload-wide "keep first/cost-preferred candidate per node" step, `bind::implement_workload`/`implement_workload_with`, would belong if a deployment still wants that exact behavior — it isn't shipped by this crate) | e.g. `control_plane::sketch_algebra::rules::bind_*` (as of this writing; expected to fold into that deployment's cost-model layer rather than stay a separate "bind" concept) |
//!
//! A related question (tracked alongside issues #6/#33): whether this
//! crate should also own a **matching** predicate — "does an already
//! *available* `Implementation` satisfy a *required* one" — the way a
//! database's materialized-view matching / "answering queries using
//! views" layer does. It owns the *question*, not an *answer*:
//! [`replacement::Matcher`] is a trait with no default implementation and
//! no shipped instance, the same shape as [`cost_model::CostModel`] and for
//! the same reason — which `Implementation`s are actually *available*
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
pub mod explanation;
pub mod grouping;
pub mod physical_operator_statistics;
pub mod physical_plan_cost_model;
pub mod query_physical_lowering;
pub mod recurrence;
pub mod replacement;
pub mod rewrite;
pub mod rollup;
pub mod summary_maintenance_cost;
pub mod summary_maintenance_dag_export;
pub mod summary_maintenance_lifecycle;
pub mod topk_reuse;

pub use accuracy::{
    AccuracyAllocation, AccuracyBudgetAllocator, AccuracyEvidenceProvider, AccuracyModel,
    CompositionShape, DefaultAccuracyModel, EqualSplitAllocator, NoAccuracyEvidence,
    PropagationStats, WorkloadAccuracyEvidence,
};
pub use accuracy_reconciliation::AccuracyReconciliationStrategy;
pub use cost_model::{CompleteSummaryCandidateEstimate, CostModel, DefaultCostModel};
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
    search_workload_with_targets, summary_candidates, GlobalSelection, ImplementError,
    Implementation, Matcher, MemoGroup, PlanSpace, Proposals, RankedGroup, RecurrenceProfileMap,
    RejectedCandidate, Replacement, ReplacementProvenance, ReplacementStrategy, ReplacementSubDAG,
    SelectedGroup, SharedSubtreeStrategy, SketchAlgorithmStrategy, TargetSubDAG,
    MAX_SEARCH_ITERATIONS,
};
pub use rewrite::{AvgToSumOverCountStrategy, SemanticEquivalentRewriteStrategy};
pub use summary_maintenance_dag_export::{
    export_summary_maintenance_plan, SummaryMaintenanceDagExport,
    SummaryMaintenanceDeploymentExport, SummaryMaintenanceLifecycleAlternativeExport,
};
pub use summary_maintenance_lifecycle::{
    global_selection_with_summary_maintenance_lifecycles,
    materialize_with_summary_maintenance_lifecycles, plan_summary_maintenance_lifecycles,
    MaterializeSummaryMaintenanceLifecycleError, SummaryMaintenanceCapabilities,
    SummaryMaintenanceDeployment, SummaryMaintenanceLifecycleAlternative,
    SummaryMaintenanceLifecycleCapabilities, SummaryMaintenanceLifecycleCostInputs,
    SummaryMaintenanceLifecyclePlan, SummaryMaintenanceLifecyclePlanError,
    SummaryMaintenanceLifecycleRejection, SummaryMaintenanceLifecycleSelectionError,
    WorkloadDemand,
};
pub use topk_reuse::TopKLimitReuseStrategy;
