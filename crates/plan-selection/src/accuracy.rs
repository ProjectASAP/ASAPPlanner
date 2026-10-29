//! Plan selection's own accuracy model: [`IgnoreAccuracyGuarantees`],
//! selected by
//! [`CostInputs::ignore_accuracy_guarantees`](asap_cost::CostInputs::ignore_accuracy_guarantees).
//! The deployment's accuracy model and evidence traits are in `asap-accuracy`
//! ([`AccuracyModel`], [`asap_accuracy::AccuracyEvidenceProvider`]).

use asap_accuracy::{AccuracyModel, BuiltinAccuracyModel};
use asap_types::ir::properties::{AccuracyGuarantee, AccuracyTarget};
use asap_types::ir::QueryRoot;

/// Accepts every query output regardless of its guarantee, for comparing
/// candidates on cost alone (e.g. a benchmark of sketch plans that have no
/// analytical guarantee); [`Self::satisfies`] is the built-in rule. Selected
/// by [`CostInputs::ignore_accuracy_guarantees`](asap_cost::CostInputs::ignore_accuracy_guarantees).
/// Executability checks still run: Count-Min over update weights not proven
/// non-negative, and exact per-series aggregates the executor has no kernel
/// for, are rejected.
/// Planning's own: the settings field is the one way to select it.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct IgnoreAccuracyGuarantees;

impl AccuracyModel for IgnoreAccuracyGuarantees {
    fn satisfies(&self, guarantee: &AccuracyGuarantee, target: &AccuracyTarget) -> bool {
        BuiltinAccuracyModel.satisfies(guarantee, target)
    }

    fn accepts_output_regardless_of_guarantee(
        &self,
        _output: &QueryRoot,
        _target: &AccuracyTarget,
    ) -> bool {
        true
    }

    /// It accepts every output, so no candidate is discarded early for
    /// its accuracy.
    fn may_accept_regardless_of_guarantee(&self) -> bool {
        true
    }
}
