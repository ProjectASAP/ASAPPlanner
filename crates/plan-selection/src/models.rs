//! The models Stage 3 checks and compares with: [`AccuracyCheckInputs`]
//! (validity) and [`CostInputs`] (the objective), and the engine's view of
//! both, [`SelectionModels`].

use asap_accuracy::{AccuracyEvidenceProvider, AccuracyModel};
use asap_cost::CostInputs;

/// Inputs of the composed-accuracy check. `raw_data_retained` is a Stage 2
/// input only; to ignore accuracy guarantees the facade passes an
/// accept-all `model` ([`asap_accuracy::IgnoreAccuracyGuarantees`]).
#[derive(Clone, Copy)]
pub struct AccuracyCheckInputs<'a> {
    /// Decides whether a candidate's accuracy guarantee meets its query's
    /// target, and whether the deployment accepts an output regardless of
    /// its guarantee.
    pub model: &'a dyn AccuracyModel,
    /// Deployment facts the guarantees need (operand domains, observed
    /// guarantees).
    pub evidence: &'a dyn AccuracyEvidenceProvider,
}

/// Stage 3's accuracy model and evidence with its cost inputs, as the
/// engine reads them.
#[derive(Clone, Copy)]
pub(crate) struct SelectionModels<'a> {
    pub accuracy: &'a dyn AccuracyModel,
    pub evidence: &'a dyn AccuracyEvidenceProvider,
    pub plan_selection: &'a CostInputs,
}

impl<'a> SelectionModels<'a> {
    pub(crate) fn new(
        accuracy_checks: &AccuracyCheckInputs<'a>,
        cost_inputs: &'a CostInputs,
    ) -> Self {
        Self {
            accuracy: accuracy_checks.model,
            evidence: accuracy_checks.evidence,
            plan_selection: cost_inputs,
        }
    }

    /// The built-in accuracy model and no evidence, under default cost
    /// inputs.
    #[cfg(test)]
    pub(crate) fn builtin() -> SelectionModels<'static> {
        static DEFAULT_COST_INPUTS: std::sync::LazyLock<CostInputs> =
            std::sync::LazyLock::new(CostInputs::default);
        SelectionModels {
            accuracy: &asap_accuracy::BuiltinAccuracyModel,
            evidence: &asap_accuracy::NoAccuracyEvidence,
            plan_selection: &DEFAULT_COST_INPUTS,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_accuracy(mut self, accuracy: &'a dyn AccuracyModel) -> Self {
        self.accuracy = accuracy;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_evidence(mut self, evidence: &'a dyn AccuracyEvidenceProvider) -> Self {
        self.evidence = evidence;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_plan_selection(mut self, plan_selection: &'a CostInputs) -> Self {
        self.plan_selection = plan_selection;
        self
    }
}
