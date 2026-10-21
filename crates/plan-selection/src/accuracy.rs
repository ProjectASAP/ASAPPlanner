//! The deployment's accuracy model, read only by plan selection (#509
//! Stage 3).
//!
//! Stage 1 composes each candidate's guarantee with the planner's built-in
//! analytical rules (`asap_logical_asap_optimization::accuracy`) and keeps a
//! candidate it has no rule for with `guarantee = None`. This stage decides,
//! with the model here, whether a guarantee meets its query's target (after
//! the data workload and the deployment's evidence,
//! [`AccuracyEvidenceProvider`], bind the statistics Stage 1 left unknown),
//! and whether the deployment accepts an output directly, whatever its
//! guarantee says.

use asap_logical_asap_optimization::accuracy::{self as analytical, OperandDomain};
use asap_types::ir::properties::{AccuracyGuarantee, AccuracyTarget};
use asap_types::ir::{OperatorNode, QueryRoot};

/// The deployment's accuracy decisions in plan selection: whether a
/// guarantee meets a query's target, and whether the deployment accepts an
/// output directly, whatever its guarantee. This crate ships
/// [`BuiltinAccuracyModel`]; a deployment with its own target check (a
/// registered cross-metric conversion, say) or its own measurements
/// implements this trait and passes it in
/// [`PlanningModels::accuracy`](crate::PlanningModels::accuracy).
/// Guarantees are composed with the built-in rules only
/// (`asap_logical_asap_optimization::accuracy::composition::propagate`).
pub trait AccuracyModel {
    /// Whether `guarantee` meets `target`: a bound within its ε, and within
    /// its δ when it states one, over the values the target's scope covers
    /// (a per-value δ does not meet a whole-result target). When the target
    /// sets a metric, the bound must be in that metric (or one with a sound
    /// direct comparison): a bound in another metric does not satisfy,
    /// however small. When it sets none, a bound in any metric does, each
    /// compared in its own metric. Unknown required dimensions fail;
    /// selection separately excludes missing accuracy evidence.
    fn satisfies(&self, guarantee: &AccuracyGuarantee, target: &AccuracyTarget) -> bool;

    /// Whether the deployment accepts `output`, one query's output in a
    /// candidate, for `target` directly, whatever its guarantee says:
    /// missing (Stage 1 had no analytical rule), met or missed. Stage 3 asks
    /// this for every query output before any guarantee check; when it
    /// returns `true`, no guarantee or evidence check runs for that output.
    /// It does not override the executability check before it: a plan that
    /// reads Count-Min (plain, CmsWithHeap or Hydra Count-Min) over update
    /// weights not proven non-negative is rejected
    /// (`RejectionCause::CountMinWeightsNotProvenNonNegative`) whatever this
    /// returns, because the executor refuses CmsWithHeap and HydraCms when it
    /// builds the plan and plain Count-Min adds a negative weight, which voids
    /// its never-underestimate bound. The default accepts
    /// none, so Stage 3 checks the Stage 1 guarantee, then the evidence
    /// ([`AccuracyEvidenceProvider::output_guarantee`]) with
    /// [`Self::satisfies`].
    fn accepts_output_regardless_of_guarantee(
        &self,
        _output: &QueryRoot,
        _target: &AccuracyTarget,
    ) -> bool {
        false
    }

    /// Whether [`Self::accepts_output_regardless_of_guarantee`] may accept
    /// some output. Plan selection discards a query's candidate before
    /// composition for a missing or unknown guarantee only when this is
    /// `false`: otherwise the model might accept the same plan with larger
    /// summaries, which sharing with another query can give it. The default
    /// is `true`, which keeps every such candidate.
    fn may_accept_regardless_of_guarantee(&self) -> bool {
        true
    }
}

/// The planner's built-in analytical rules as an [`AccuracyModel`]: the
/// conservative target check Stage 1's
/// `accuracy::satisfies` makes, and no output accepted regardless of its
/// guarantee.
/// The default in [`PlanningModels::builtin`](crate::PlanningModels::builtin).
#[derive(Debug, Default, Clone, Copy)]
pub struct BuiltinAccuracyModel;

impl AccuracyModel for BuiltinAccuracyModel {
    fn satisfies(&self, guarantee: &AccuracyGuarantee, target: &AccuracyTarget) -> bool {
        analytical::satisfies(guarantee, target)
    }

    fn may_accept_regardless_of_guarantee(&self) -> bool {
        false
    }
}

/// The deployment's planning-time accuracy evidence, read only by plan
/// selection: enforced operand domains the propagation rules need, and
/// empirical guarantees for whole query outputs. Passed in
/// [`PlanningModels::evidence`](crate::PlanningModels::evidence). The trait
/// moved here from `asap_logical_asap_optimization::accuracy`.
pub trait AccuracyEvidenceProvider {
    /// Enforced domain of `operand`'s true values ([`OperandDomain`]), scoped
    /// to this complete expression, including its source, filters, grouping
    /// and window: a quantile ratio's operands, or the operands of a
    /// product or quotient of estimates. `None` means unknown, including
    /// emptiness.
    fn operand_domain(&self, _operand: &OperatorNode) -> Option<OperandDomain> {
        None
    }

    /// The deployment's empirical accuracy for one candidate's whole query
    /// output, as a guarantee. Plan selection (#509 Stage 3) accepts a
    /// candidate whose output has no theoretical guarantee that meets the
    /// target (e.g. a difference of two quantile estimates) when this shows
    /// the output meets it. Stage 1 never reads it: root guarantees there
    /// come from the built-in analytical rules. `None` means no evidence.
    fn output_guarantee(&self, _output: &QueryRoot) -> Option<AccuracyGuarantee> {
        None
    }

    /// Whether this provider may return any evidence: an operand domain or
    /// an output guarantee. Plan selection discards a query's candidate
    /// before composition for a missing or unknown guarantee only when this
    /// is `false`: otherwise evidence might exist for the same plan with
    /// larger summaries, which sharing with another query can give it. The
    /// default is `true`, which keeps every such candidate.
    fn may_provide_evidence(&self) -> bool {
        true
    }
}

/// No deployment evidence: every operand domain and output guarantee is
/// unknown. The default in [`PlanningModels::builtin`](crate::PlanningModels::builtin).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoAccuracyEvidence;

impl AccuracyEvidenceProvider for NoAccuracyEvidence {
    fn may_provide_evidence(&self) -> bool {
        false
    }
}
