//! `asap-logical-optimizer` — #509 Stage 1: logical candidate generation.
//!
//! It takes the pre-ASAP [`OperatorNode`](asap_types::ir::OperatorNode) DAGs a
//! front end produces and proposes the logical alternatives for each target
//! sub-DAG: which summary (if any) realizes each approximate intent, and which
//! semantic rewrites, roll-ups, groupings and exact compositions apply. It
//! never prices a plan: only Stage 3 uses the cost model (#572, decision
//! Q36(a)). Cargo enforces the stage order: this crate depends only on
//! `asap-types`, never on a front end, a later stage or the executor.
//!
//! - [`pass1`] — local alternatives per target sub-DAG.
//! - [`pass2`] — ASAP-aware sharing across targets.
//! - [`accuracy`] — the analytical accuracy model: per-family error bounds and
//!   sizing ([`accuracy::estimators`]), propagation through compositions
//!   ([`accuracy::composition`]) and error-budget allocation
//!   ([`accuracy::allocation`]). Candidates no analytical rule can prove
//!   invalid are kept.
//!
//! **Common sub-expression elimination (CSE) of identical sub-DAGs is not
//! implemented here.** It runs over the pre-ASAP IR itself
//! (`asap_types::ir::cse`, issues #222/#223). Pass 2's identical-expression
//! rule ([`pass2::identical_expressions`]) decides when to use it: the stage
//! pipeline keeps a variant with and without it. Pass 2 also recognizes
//! sharing that is invisible at that level, such as `Quantile(x, 0.99)` and
//! `Quantile(x, 0.95)` reading one built sketch.
//!
//! ## Candidate search
//!
//! Search returns [`CandidateLogicalASAPDAGs`](pass1::replacement::CandidateLogicalASAPDAGs),
//! a compact logical choice space with one [`TargetSubDAGCandidates`] per
//! target sub-DAG. [`ReplacementStrategy`] implementations propose local
//! alternatives; search applies the applicable semantic and accuracy checks.
//! Candidate presence does not certify physical deployability or an unknown
//! accuracy guarantee. [`GlobalSelection`] assembles a DAG from given per-target
//! choices; choosing them is a later stage's job.
//!
//! | Term | Meaning | Entry point |
//! |---|---|---|
//! | Realization | Enumerate the physical forms for one aggregate intent | `pass1::replacement::realizations_for_intent` |
//! | Replacement | Construct each candidate summary sub-DAG | [`ASAPStrategies`] |
//! | Search | Enumerate alternatives across a workload | [`search_workload`] |
//! | Local candidates | The #509 stage pipeline's Stage 1 entry point | [`pass1::logical_candidates`] |
//!
//! [`Matcher`] asks whether an already available `Realization` satisfies a
//! required one. It has no shipped implementation: which realizations are
//! available is a deployment's concern.
//!
//! [`explanation`](pass1::explanation) reports, for each discovered target
//! with a non-trivial candidate list, why a replacement exists, reusing the
//! candidate's own rationale.

pub mod accuracy;
pub mod pass1;
pub mod pass2;
#[cfg(test)]
mod test_support;

pub use accuracy::{
    AccuracyAllocation, AccuracyBudgetAllocator, AccuracyEvidenceProvider, AccuracyModel,
    CompositionShape, DefaultAccuracyModel, EqualSplitAllocator, NoAccuracyEvidence,
    PropagationStats, WorkloadAccuracyEvidence,
};
pub use pass1::exact_composition::{
    ExactComposition, ExactCompositionStrategy, OperationPlacement,
};
pub use pass1::explanation::{
    explain_replacements, explain_replacements_with, ExplanationKind, ReplacementExplanation,
};
pub use pass1::grouping::{has_subpopulations, HydraGroupingStrategy};
pub use pass1::replacement::{
    default_strategies, is_logical_rewrite, search_workload, search_workload_with,
    search_workload_with_targets, summary_candidates, ASAPStrategies, CandidateLogicalASAPDAGs,
    GlobalSelection, Matcher, Proposals, Realization, RealizationError, RejectedCandidate,
    Replacement, ReplacementProvenance, ReplacementStrategy, ReplacementSubDAG,
    SharedSubDAGStrategy, TargetSubDAG, TargetSubDAGCandidates, TargetSubDAGSelection,
    MAX_SEARCH_ITERATIONS,
};
pub use pass1::rewrite::{AvgToSumOverCountStrategy, SemanticEquivalentRewriteStrategy};
pub use pass2::reconciliation::AccuracyReconciliationStrategy;
pub use pass2::topk_reuse::TopKLimitReuseStrategy;
