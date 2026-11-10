//! The planning models the facade splits by stage: the deployment's
//! measured accuracy, cost inputs and capabilities ([`PlanningModels`]).

use asap_accuracy::{AccuracyEvidence, NoMeasuredGuarantees};
use asap_cost::CostInputs;
use asap_types::deployment_capabilities::DeploymentCapabilities;

static DEFAULT_COST_INPUTS: std::sync::LazyLock<CostInputs> =
    std::sync::LazyLock::new(CostInputs::default);

static DEFAULT_DEPLOYMENT: std::sync::LazyLock<DeploymentCapabilities> =
    std::sync::LazyLock::new(DeploymentCapabilities::default);

/// Planning logic, as opposed to the scoped facts it consumes: a model can have
/// a built-in default, evidence about a particular deployment cannot.
///
/// The facade splits them by stage: Stage 1 reads the supported summaries
/// of `deployment_capabilities`, Pass 2 the accuracy `evidence`, Stage 2 the
/// deployment's retention, and Stage 3 (plan selection) compares candidates
/// under `cost_inputs` ([`CostInputs`]).
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct PlanningModels<'a> {
    /// The deployment's measured accuracy, which Pass 2 reads before each
    /// summary's theoretical guarantee; none by default.
    pub evidence: &'a dyn AccuracyEvidence,
    /// The deployment's cost inputs: the cost model (default
    /// cost model 1 with illustrative unit costs), the resource speed and
    /// the non-repeating query horizon.
    pub cost_inputs: &'a CostInputs,
    /// What the deployment's executor can run and keep: the summaries it
    /// supports (Stage 1), whether it retains raw data (Stage 3) and
    /// whether it keeps query-time results across evaluations (Stage 2);
    /// default [`DeploymentCapabilities::default`].
    pub deployment_capabilities: &'a DeploymentCapabilities,
    /// Do not enforce accuracy: the facade gives every query "no accuracy
    /// target" before Pass 2, so no readout gets a required accuracy and
    /// every θ connects (a top-k's heap still holds k). For comparing
    /// candidates on cost alone, e.g. a benchmark of sketch plans with no
    /// analytical guarantee. Default `false`.
    pub ignore_accuracy_guarantees: bool,
}

impl<'a> PlanningModels<'a> {
    /// This evidence with the default plan selection settings.
    pub fn new(evidence: &'a dyn AccuracyEvidence) -> Self {
        Self {
            evidence,
            cost_inputs: &DEFAULT_COST_INPUTS,
            deployment_capabilities: &DEFAULT_DEPLOYMENT,
            ignore_accuracy_guarantees: false,
        }
    }

    /// The built-in models: no accuracy evidence.
    pub fn builtin() -> PlanningModels<'static> {
        PlanningModels::new(&NoMeasuredGuarantees)
    }

    /// These models, enforcing no accuracy target when `ignore`
    /// (`ignore_accuracy_guarantees`).
    pub fn with_ignore_accuracy_guarantees(mut self, ignore: bool) -> Self {
        self.ignore_accuracy_guarantees = ignore;
        self
    }

    pub fn with_evidence(mut self, evidence: &'a dyn AccuracyEvidence) -> Self {
        self.evidence = evidence;
        self
    }

    /// These models with the deployment's cost inputs (cost
    /// model, resource speed, non-repeating query horizon).
    pub fn with_cost_inputs(mut self, cost_inputs: &'a CostInputs) -> Self {
        self.cost_inputs = cost_inputs;
        self
    }

    /// These models with the deployment's capabilities (supported
    /// summaries, raw-data and query-time retention).
    pub fn with_deployment_capabilities(
        mut self,
        deployment_capabilities: &'a DeploymentCapabilities,
    ) -> Self {
        self.deployment_capabilities = deployment_capabilities;
        self
    }
}
