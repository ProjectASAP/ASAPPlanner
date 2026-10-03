//! The pluggable optimization pass (issue #430).
//!
//! An [`OptimizationPass`] is the whole optimization stage behind one
//! signature: pre-ASAP IR in, post-ASAP DAG out. The trait deliberately names
//! none of this crate's two-phase vocabulary — no `CandidateLogicalASAPDAGs`, no
//! `TargetSubDAGCandidates`, no `ReplacementStrategy` — so an algorithm with no
//! candidate-generation phase at all (a greedy MQO loop, say) can implement it
//! without pretending to have phases it does not have. The shipped algorithm is
//! one implementation, [`MajorPass`].
//!
//! Call [`optimize`] rather than [`OptimizationPass::optimize`] directly: it
//! validates the input once for every pass and checks the output contract that
//! downstream consumers rely on.

mod major;

use std::collections::BTreeMap;
use std::rc::Rc;

use asap_types::ir::OperatorNode;
use asap_types::parsed_workload::ParsedWorkload;
use asap_types::workload::WorkloadError;

use crate::accuracy::{
    AccuracyEvidenceProvider, AccuracyModel, DefaultAccuracyModel, NoAccuracyEvidence,
};
use crate::cost_model::{CostModel, DefaultCostModel};
use crate::recurrence::Horizon;
use crate::replacement::RealizationError;
use crate::summary_maintenance_lifecycle::{
    SummaryMaintenanceLifecycleAssemblyError, SummaryMaintenanceLifecycleCapabilities,
    SummaryMaintenanceLifecyclePlan, SummaryMaintenanceLifecycleSelectionError,
};

pub use major::MajorPass;

static DEFAULT_COST_MODEL: DefaultCostModel = DefaultCostModel;
static DEFAULT_ACCURACY_MODEL: DefaultAccuracyModel = DefaultAccuracyModel;
static NO_ACCURACY_EVIDENCE: NoAccuracyEvidence = NoAccuracyEvidence;

// ── Input ────────────────────────────────────────────────────────────────

/// Planning logic, as opposed to the scoped facts it consumes: a model can have
/// a built-in default, evidence about a particular deployment cannot.
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct PlanningModels<'a> {
    pub cost: &'a dyn CostModel,
    pub accuracy: &'a dyn AccuracyModel,
    pub evidence: &'a dyn AccuracyEvidenceProvider,
    pub capabilities: SummaryMaintenanceLifecycleCapabilities,
}

impl<'a> PlanningModels<'a> {
    pub fn new(
        cost: &'a dyn CostModel,
        accuracy: &'a dyn AccuracyModel,
        evidence: &'a dyn AccuracyEvidenceProvider,
    ) -> Self {
        Self {
            cost,
            accuracy,
            evidence,
            capabilities: SummaryMaintenanceLifecycleCapabilities::ALL,
        }
    }

    /// The built-in models. `DefaultCostModel` does not override
    /// `estimate_cost`, so this configuration ranks structurally and is not a
    /// measured deployment cost.
    pub fn builtin() -> PlanningModels<'static> {
        PlanningModels {
            cost: &DEFAULT_COST_MODEL,
            accuracy: &DEFAULT_ACCURACY_MODEL,
            evidence: &NO_ACCURACY_EVIDENCE,
            capabilities: SummaryMaintenanceLifecycleCapabilities::ALL,
        }
    }

    pub fn with_cost(mut self, cost: &'a dyn CostModel) -> Self {
        self.cost = cost;
        self
    }

    pub fn with_accuracy(mut self, accuracy: &'a dyn AccuracyModel) -> Self {
        self.accuracy = accuracy;
        self
    }

    pub fn with_evidence(mut self, evidence: &'a dyn AccuracyEvidenceProvider) -> Self {
        self.evidence = evidence;
        self
    }

    pub fn with_capabilities(
        mut self,
        capabilities: SummaryMaintenanceLifecycleCapabilities,
    ) -> Self {
        self.capabilities = capabilities;
        self
    }
}

/// Supplying this asks the pass to also decide summary maintenance versus raw
/// recomputation; leaving it out asks only for the logical DAG.
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct LifecycleInput {
    /// Planning clock, Unix milliseconds.
    pub now_ms: u64,
    /// Seconds. Required to turn recurring demand into a finite total.
    pub horizon: Option<Horizon>,
}

impl LifecycleInput {
    pub fn new(now_ms: u64) -> Self {
        Self {
            now_ms,
            horizon: None,
        }
    }

    pub fn with_horizon(mut self, horizon: Horizon) -> Self {
        self.horizon = Some(horizon);
        self
    }
}

#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct OptimizationInput<'a> {
    pub workload: &'a ParsedWorkload,
    pub models: PlanningModels<'a>,
    /// Every plan carries the maintenance-versus-recomputation decision, so
    /// the planning clock is always required. Runtime capabilities are in
    /// `models` with the deployment's cost and accuracy models.
    pub lifecycle: LifecycleInput,
}

impl<'a> OptimizationInput<'a> {
    pub fn new(
        workload: &'a ParsedWorkload,
        models: PlanningModels<'a>,
        lifecycle: LifecycleInput,
    ) -> Self {
        Self {
            workload,
            models,
            lifecycle,
        }
    }

    pub fn validate(&self) -> Result<(), OptimizationInputError> {
        self.workload
            .validate()
            .map_err(OptimizationInputError::Workload)?;
        if let Some(horizon) = self.lifecycle.horizon {
            if !horizon.0.is_finite() || horizon.0 <= 0.0 {
                return Err(OptimizationInputError::InvalidHorizon(horizon.0));
            }
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OptimizationInputError {
    #[error("workload: {0}")]
    Workload(WorkloadError),
    #[error("planning horizon must be finite and positive, got {0}")]
    InvalidHorizon(f64),
}

// ── Output ───────────────────────────────────────────────────────────────

/// One query's selected post-ASAP DAG plus the maintenance decisions taken
/// for it. The DAG is `plan.root`.
#[derive(Debug, Clone)]
pub struct QueryLifecyclePlan {
    /// Index into `QueryWorkload::entries()`.
    pub entry_index: usize,
    pub plan: SummaryMaintenanceLifecyclePlan,
}

/// One multi-root workload DAG with query/lifecycle bindings in entry order;
/// [`check_contract`] enforces that.
///
/// Plans are not deduplicated across entries: a summary state that several
/// queries share appears in each of their plans as the same `Rc` (with the
/// same lifecycle), so a consumer that deploys or costs the workload must
/// dedupe deployments by `Rc::ptr_eq` on the summary node.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PlanOutput {
    pub plans: Vec<QueryLifecyclePlan>,
    /// Exact scalar expressions, keyed by workload entry; embedded plan reads remain visible.
    pub scalar_roots: Vec<(usize, asap_types::ir::ScalarExpr)>,
}

impl PlanOutput {
    pub fn new(plans: Vec<QueryLifecyclePlan>) -> Self {
        Self {
            plans,
            scalar_roots: Vec::new(),
        }
    }

    /// Entry indices in output order.
    pub fn entry_indices(&self) -> Vec<usize> {
        let mut indices: Vec<_> = self
            .plans
            .iter()
            .map(|p| p.entry_index)
            .chain(self.scalar_roots.iter().map(|(i, _)| *i))
            .collect();
        if !self.scalar_roots.is_empty() {
            indices.sort_unstable();
        }
        indices
    }

    /// All query roots in workload order, including standalone scalars.
    pub fn roots(&self) -> Vec<asap_types::ir::QueryRoot> {
        let mut roots: Vec<_> = self
            .plans
            .iter()
            .map(|p| {
                (
                    p.entry_index,
                    asap_types::ir::QueryRoot::Operator(Rc::clone(&p.plan.root)),
                )
            })
            .chain(
                self.scalar_roots
                    .iter()
                    .map(|(i, expr)| (*i, asap_types::ir::QueryRoot::Scalar(expr.clone()))),
            )
            .collect();
        roots.sort_by_key(|(i, _)| *i);
        roots.into_iter().map(|(_, root)| root).collect()
    }

    /// The selected operator roots. Use `roots()` to include scalar queries.
    pub fn operator_roots(&self) -> Vec<Rc<OperatorNode>> {
        self.plans.iter().map(|p| Rc::clone(&p.plan.root)).collect()
    }

    /// Unique operators in the entire workload DAG, including scalar-plan dependencies.
    /// Several query roots can reach the same operator; it is returned once.
    pub fn operators(&self) -> Vec<Rc<OperatorNode>> {
        let mut seen = std::collections::HashSet::new();
        let mut nodes = Vec::new();
        for root in self.roots() {
            let inputs = match root {
                asap_types::ir::QueryRoot::Operator(node) => vec![node],
                asap_types::ir::QueryRoot::Scalar(expr) => {
                    expr.operator_refs().into_iter().cloned().collect()
                }
            };
            for input in inputs {
                for node in OperatorNode::reachable(&input) {
                    if seen.insert(Rc::as_ptr(&node)) {
                        nodes.push(node);
                    }
                }
            }
        }
        nodes
    }

    pub fn len(&self) -> usize {
        self.plans.len() + self.scalar_roots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OptimizeError {
    #[error("optimization input: {0}")]
    Input(#[from] OptimizationInputError),
    #[error("entry {entry_index}: {source}")]
    Realization {
        entry_index: usize,
        source: RealizationError,
    },
    #[error("summary-maintenance-lifecycle selection: {0}")]
    LifecycleSelection(SummaryMaintenanceLifecycleSelectionError),
    #[error("entry {entry_index}: {source}")]
    LifecycleAssembly {
        entry_index: usize,
        source: SummaryMaintenanceLifecycleAssemblyError,
    },
    /// The pass returned something the downstream contract forbids. This is a
    /// defect in the pass, not in its input.
    #[error("pass `{pass}` violated the output contract: {detail}")]
    ContractViolation { pass: &'static str, detail: String },
}

// ── The pass ─────────────────────────────────────────────────────────────

/// The optimization stage, end to end.
///
/// Implementors are free to ignore any part of [`OptimizationInput`] they do
/// not use, but must uphold the contract [`check_contract`] enforces.
pub trait OptimizationPass {
    /// Registry key, also used in diagnostics.
    fn name(&self) -> &'static str;

    fn optimize(&self, input: OptimizationInput<'_>) -> Result<PlanOutput, OptimizeError>;
}

/// Run `pass` over `input`: validate once, then check the output contract.
///
/// Prefer this over calling [`OptimizationPass::optimize`] directly. A pass is
/// third-party code, but `ASAPQuery-backend` and `ASAPCollector` read every
/// pass's output against the same contract; checking here means a defective
/// pass fails at the boundary instead of corrupting a deployment.
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

/// Structural checks only — that every query is accounted for, in order.
/// Whether the pass chose *well* is not checked.
fn check_contract(
    output: &PlanOutput,
    pass: &'static str,
    expected_len: usize,
) -> Result<(), OptimizeError> {
    let violation = |detail: String| OptimizeError::ContractViolation { pass, detail };

    if output.len() != expected_len {
        return Err(violation(format!(
            "{} plan(s) for {expected_len} workload entry/entries",
            output.len()
        )));
    }
    for (position, entry_index) in output.entry_indices().into_iter().enumerate() {
        if entry_index != position {
            return Err(violation(format!(
                "plan at position {position} carries entry_index {entry_index}"
            )));
        }
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

    /// Only [`MajorPass`], under the name `major`.
    pub fn with_builtin() -> Self {
        let mut registry = Self::new();
        registry
            .register(Box::new(MajorPass))
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

    impl OptimizationPass for Stub {
        fn name(&self) -> &'static str {
            self.0
        }
        fn optimize(&self, _input: OptimizationInput<'_>) -> Result<PlanOutput, OptimizeError> {
            Ok(PlanOutput::new(Vec::new()))
        }
    }

    /// Dropping a query is rejected: the output is positionally aligned with
    /// the workload's entries, so a short vector is not a partial result.
    #[test]
    fn contract_rejects_a_plan_count_that_does_not_cover_every_entry() {
        let output = PlanOutput::new(Vec::new());
        let err = check_contract(&output, "stub", 2).unwrap_err();
        assert!(matches!(
            err,
            OptimizeError::ContractViolation { pass: "stub", .. }
        ));
    }

    /// The matching count passes.
    #[test]
    fn contract_accepts_an_empty_workload() {
        let output = PlanOutput::new(Vec::new());
        assert!(check_contract(&output, "stub", 0).is_ok());
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

    /// The builtin registry resolves `major`, and names come back sorted so a
    /// sweep over every registered pass is reproducible.
    #[test]
    fn registry_resolves_builtin_and_lists_names_in_order() {
        let mut registry = PassRegistry::with_builtin();
        registry.register(Box::new(Stub("alpha"))).unwrap();
        assert!(registry.get("major").is_some());
        assert!(registry.get("absent").is_none());
        assert_eq!(registry.names().collect::<Vec<_>>(), vec!["alpha", "major"]);
    }
}
