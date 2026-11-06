//! The pluggable optimization pass (issue #430), and the planning models
//! the facade splits by stage.
//!
//! An [`OptimizationPass`] is the whole optimization stage behind one
//! signature: pre-ASAP IR in, post-ASAP DAG out. The trait deliberately names
//! none of the stages' candidate types, so an algorithm with no
//! candidate-generation phase at all (a greedy MQO loop, say) can implement it
//! without pretending to have phases it does not have. The shipped algorithm is
//! one implementation, [`Stage1ToStage3Pass`]: the planning stages in order
//! ([`run_planning`](crate::run_planning)).
//!
//! Call [`optimize`] rather than [`OptimizationPass::optimize`] directly: it
//! validates the input once for every pass and checks the output contract that
//! downstream consumers rely on.

use std::collections::BTreeMap;

use asap_types::workload::parsed_workload::ParsedWorkload;
use asap_types::workload::WorkloadError;

use crate::run::{SelectionError, Stage1ToStage3Pass};
use asap_accuracy::{AccuracyEvidence, NoMeasuredGuarantees};
use asap_cost::CostInputs;
use asap_stage1_pass1_per_computation_candidates::PerComputationCandidateError;
use asap_types::deployment_capabilities::DeploymentCapabilities;
use asap_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPDAGValidationError};

static DEFAULT_COST_INPUTS: std::sync::LazyLock<CostInputs> =
    std::sync::LazyLock::new(CostInputs::default);
static DEFAULT_DEPLOYMENT: std::sync::LazyLock<DeploymentCapabilities> =
    std::sync::LazyLock::new(DeploymentCapabilities::default);

// ── Input ────────────────────────────────────────────────────────────────

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

#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct OptimizationInput<'a> {
    pub workload: &'a ParsedWorkload,
    pub models: PlanningModels<'a>,
}

impl<'a> OptimizationInput<'a> {
    pub fn new(workload: &'a ParsedWorkload, models: PlanningModels<'a>) -> Self {
        Self { workload, models }
    }

    pub fn validate(&self) -> Result<(), OptimizationInputError> {
        self.workload
            .validate()
            .map_err(OptimizationInputError::Workload)
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OptimizationInputError {
    #[error("workload: {0}")]
    Workload(WorkloadError),
}

// ── Output ───────────────────────────────────────────────────────────────

/// The selected plan: one `PhysicalASAPDAG`, the only plan structure, in
/// memory and, serialized, in the plan file installed on the executor. Its
/// `roots` answer the operator queries and its `scalar_roots` the standalone
/// scalar queries, in entry order; `check_contract` enforces that.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PlanOutput {
    pub plan: PhysicalASAPDAG,
}

impl PlanOutput {
    pub fn new(plan: PhysicalASAPDAG) -> Self {
        Self { plan }
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OptimizeError {
    #[error("optimization input: {0}")]
    Input(#[from] OptimizationInputError),
    #[error("logical ASAP-aware optimization: {0}")]
    PerComputationCandidates(#[from] PerComputationCandidateError),
    #[error("plan selection: {0}")]
    Selection(#[from] SelectionError),
    /// The pass returned something the downstream contract forbids. This is a
    /// defect in the pass, not in its input.
    #[error("pass `{pass}` violated the output contract: {detail}")]
    ContractViolation { pass: &'static str, detail: String },
    /// The pass returned an invalid DAG: a defect in the pass.
    #[error("pass `{pass}` returned an invalid plan: {error}")]
    InvalidPlan {
        pass: &'static str,
        error: PhysicalASAPDAGValidationError,
    },
}

// ── The pass ─────────────────────────────────────────────────────────────

/// The optimization stage, end to end.
///
/// Implementors are free to ignore any part of [`OptimizationInput`] they do
/// not use, but must uphold the contract `check_contract` enforces.
pub trait OptimizationPass {
    /// Registry key, also used in diagnostics.
    fn name(&self) -> &'static str;

    fn optimize(&self, input: OptimizationInput<'_>) -> Result<PlanOutput, OptimizeError>;
}

/// Run `pass` over `input`: validate once, then check the output contract.
///
/// Prefer this over calling [`OptimizationPass::optimize`] directly. A pass is
/// third-party code, but every deployment runtime reads every pass's output
/// against the same contract; checking here means a defective pass fails at
/// the boundary instead of corrupting a deployment.
pub fn optimize(
    pass: &dyn OptimizationPass,
    input: OptimizationInput<'_>,
) -> Result<PlanOutput, OptimizeError> {
    input.validate()?;
    let expected_len = input.workload.len();

    let output = pass.optimize(input)?;
    check_contract(&output, pass.name(), expected_len)?;
    Ok(output)
}

/// Structural checks only — that every query is answered once, by an
/// operator root or the scalar root keyed with its entry index, and that
/// the DAG is valid. Whether the pass chose *well* is not checked.
fn check_contract(
    output: &PlanOutput,
    pass: &'static str,
    expected_len: usize,
) -> Result<(), OptimizeError> {
    let plan = &output.plan;
    let answered = plan.roots.len() + plan.scalar_roots.len();
    if answered != expected_len {
        return Err(OptimizeError::ContractViolation {
            pass,
            detail: format!("{answered} plan root(s) for {expected_len} workload entry/entries"),
        });
    }
    // An empty workload's plan has no roots, which `validate` rejects.
    if expected_len > 0 {
        plan.validate()
            .map_err(|error| OptimizeError::InvalidPlan { pass, error })?;
    }
    Ok(())
}

// ── Registry ─────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
#[error("a pass named `{0}` is already registered")]
pub struct PassNameConflict(pub &'static str);

/// Name-keyed lookup, for callers that choose a pass from a string: a CLI flag,
/// a config file, or a harness sweeping every registered pass over one input.
///
/// Deliberately caller-owned rather than a link-time global: two tests in one
/// binary must not see each other's registrations.
#[derive(Default)]
pub struct PassRegistry {
    passes: BTreeMap<&'static str, Box<dyn OptimizationPass>>,
}

impl PassRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Only [`Stage1ToStage3Pass`], under the name `stage1-to-stage3`.
    pub fn with_builtin() -> Self {
        let mut registry = Self::new();
        registry
            .register(Box::new(Stage1ToStage3Pass))
            .expect("empty registry cannot conflict");
        registry
    }

    /// Keyed by `pass.name()`. Registering a name twice is an error rather
    /// than an overwrite: silently replacing one baseline with another would
    /// make a comparison run measure the same pass twice.
    pub fn register(&mut self, pass: Box<dyn OptimizationPass>) -> Result<(), PassNameConflict> {
        let name = pass.name();
        if self.passes.contains_key(name) {
            return Err(PassNameConflict(name));
        }
        self.passes.insert(name, pass);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&dyn OptimizationPass> {
        self.passes.get(name).map(AsRef::as_ref)
    }

    /// Registered names, sorted.
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.passes.keys().copied()
    }

    pub fn len(&self) -> usize {
        self.passes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.passes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub(&'static str);

    fn empty_plan() -> PlanOutput {
        PlanOutput::new(PhysicalASAPDAG {
            nodes: vec![],
            edges: vec![],
            roots: vec![],
            scalar_roots: vec![],
        })
    }

    impl OptimizationPass for Stub {
        fn name(&self) -> &'static str {
            self.0
        }
        fn optimize(&self, _input: OptimizationInput<'_>) -> Result<PlanOutput, OptimizeError> {
            Ok(empty_plan())
        }
    }

    /// Dropping a query is rejected: the plan's roots answer the workload's
    /// entries in order, so a plan with fewer roots is not a partial result.
    #[test]
    fn contract_rejects_a_plan_count_that_does_not_cover_every_entry() {
        let err = check_contract(&empty_plan(), "stub", 2).unwrap_err();
        assert!(matches!(
            err,
            OptimizeError::ContractViolation { pass: "stub", .. }
        ));
    }

    /// A scalar root keyed by an entry index the workload does not have is
    /// rejected as an invalid plan.
    #[test]
    fn contract_rejects_a_scalar_root_keyed_beyond_the_entries() {
        use asap_types::ir::scalar::ScalarValue;
        use asap_types::ir::ScalarExpr;
        let mut output = empty_plan();
        output
            .plan
            .scalar_roots
            .push((1, ScalarExpr::Literal(ScalarValue::Float64(1.0))));
        let err = check_contract(&output, "stub", 1).unwrap_err();
        assert!(matches!(
            err,
            OptimizeError::InvalidPlan {
                pass: "stub",
                error: PhysicalASAPDAGValidationError::ScalarRootPosition(1)
            }
        ));
    }

    /// The matching count passes.
    #[test]
    fn contract_accepts_an_empty_workload() {
        assert!(check_contract(&empty_plan(), "stub", 0).is_ok());
    }

    /// Registering a name twice fails instead of overwriting, so a comparison
    /// run cannot silently measure one pass twice.
    #[test]
    fn registry_refuses_a_duplicate_name() {
        let mut registry = PassRegistry::with_builtin();
        assert!(registry.register(Box::new(Stub("greedy"))).is_ok());
        let err = registry.register(Box::new(Stub("greedy"))).unwrap_err();
        assert_eq!(err.0, "greedy");
    }

    /// The builtin registry resolves `stage1-to-stage3`, and names come back
    /// sorted so a sweep over every registered pass is reproducible.
    #[test]
    fn registry_resolves_builtin_and_lists_names_in_order() {
        let mut registry = PassRegistry::with_builtin();
        registry.register(Box::new(Stub("alpha"))).unwrap();
        assert!(registry.get("stage1-to-stage3").is_some());
        assert!(registry.get("absent").is_none());
        assert_eq!(
            registry.names().collect::<Vec<_>>(),
            vec!["alpha", "stage1-to-stage3"]
        );
    }
}
