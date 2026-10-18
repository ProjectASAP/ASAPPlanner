//! The deployment's accuracy model, read only by plan selection (#509
//! Stage 3).
//!
//! Stage 1 composes each candidate's guarantee with the planner's built-in
//! analytical rules (`asap_logical_asap_optimization::accuracy`) and keeps a
//! candidate it has no rule for with `guarantee = None`. This stage decides,
//! with the model here, whether a guarantee meets its query's target and
//! how guarantees compose once the data workload and the deployment's
//! evidence bind the statistics Stage 1 left unknown.

use asap_logical_asap_optimization::accuracy::{self as analytical, PropagationStats};
use asap_types::ir::properties::{
    AccuracyError, AccuracyGuarantee, AccuracyTarget, CompositionOperator,
};

/// The deployment-extensible accuracy algebra plan selection uses. This
/// crate ships [`DefaultAccuracyModel`]; a deployment with a proof for a
/// composition the default rejects (a registered cross-metric conversion,
/// say) implements this trait and passes it in
/// [`PlanningModels::accuracy`](crate::PlanningModels::accuracy).
pub trait AccuracyModel {
    /// Compose `inputs`' guarantees (in the parent's child order) with the
    /// parent's own `local` guarantee under `op`, given the statistics
    /// plan selection bound (`stats`). `Err` is the fail-closed answer: no
    /// registered rule, or a missing input guarantee.
    fn propagate(
        &self,
        op: &CompositionOperator,
        inputs: &[AccuracyGuarantee],
        local: Option<&AccuracyGuarantee>,
        stats: &PropagationStats,
    ) -> Result<AccuracyGuarantee, AccuracyError>;

    /// Whether `guarantee` meets `target`: a bound within its ε, and within
    /// its δ when it states one, over the values the target's scope covers
    /// (a per-value δ does not meet a whole-result target). When the target
    /// sets a metric, the bound must be in that metric (or one with a sound
    /// direct comparison): a bound in another metric does not satisfy,
    /// however small. When it sets none, a bound in any metric does, each
    /// compared in its own metric. Unknown required dimensions fail;
    /// selection separately excludes missing accuracy evidence.
    fn satisfies(&self, guarantee: &AccuracyGuarantee, target: &AccuracyTarget) -> bool;
}

/// The planner's built-in analytical rules as an [`AccuracyModel`]: the
/// same composition rules and conservative target check Stage 1 uses. The
/// default in [`PlanningModels::builtin`](crate::PlanningModels::builtin).
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultAccuracyModel;

impl AccuracyModel for DefaultAccuracyModel {
    fn propagate(
        &self,
        op: &CompositionOperator,
        inputs: &[AccuracyGuarantee],
        local: Option<&AccuracyGuarantee>,
        stats: &PropagationStats,
    ) -> Result<AccuracyGuarantee, AccuracyError> {
        analytical::propagate(op, inputs, local, stats)
    }

    fn satisfies(&self, guarantee: &AccuracyGuarantee, target: &AccuracyTarget) -> bool {
        analytical::satisfies(guarantee, target)
    }
}
